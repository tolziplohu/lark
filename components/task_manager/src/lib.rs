use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use url::Url;

use languageserver_types::Position;

pub type TaskId = usize;

enum MsgFromManager<T> {
    Shutdown,
    Message(T),
}

pub enum LspRequest {
    TypeForPos(TaskId, Url, Position),
    OpenFile(Url, String),
    Initialize(TaskId),
}

pub enum LspResponse {
    Type(TaskId, String),
    Completions(TaskId, Vec<(String, String)>),
    Initialized(TaskId),
}

pub enum MsgToManager {
    QueryResponse(QueryResponse),
    LspRequest(LspRequest),
    Cancel(TaskId),
    Shutdown,
}

pub enum QueryRequest {
    /// URI followed by contents
    OpenFile(Url, String),
    EditFile(String),
    TypeAtPosition(TaskId, Url, Position),
}

pub enum QueryResponse {
    Type(TaskId, String),
}

enum RecipeStep {
    GetTextForFile,

    RespondWithType,
    RespondWithInitialized,
}

pub trait Actor {
    type InMessage: Send + Sync + 'static;
    type OutMessage: Send + Sync + 'static;

    fn startup(&mut self, send_channel: Box<dyn Fn(Self::OutMessage) -> () + Send>);
    fn receive_message(&mut self, message: Self::InMessage);
    fn shutdown(&mut self);
}

pub struct ActorControl<MessageType: Send + Sync + 'static> {
    pub channel: Sender<MessageType>,
    pub join_handle: std::thread::JoinHandle<()>,
}

pub struct TaskManager {
    live_recipes: HashMap<TaskId, Vec<RecipeStep>>,
    receive_channel: Receiver<MsgToManager>,

    /// Control points to communicate with other subsystems
    query_system: ActorControl<MsgFromManager<QueryRequest>>,
    lsp_responder: ActorControl<MsgFromManager<LspResponse>>,
}

impl TaskManager {
    pub fn spawn(
        mut query_system: impl Actor<InMessage = QueryRequest, OutMessage = QueryResponse>
            + Send
            + 'static,
        mut lsp_responder: impl Actor<InMessage = LspResponse> + Send + 'static,
    ) -> ActorControl<MsgToManager> {
        let (manager_tx, manager_rx) = channel();

        let manager_tx_clone = manager_tx.clone();

        query_system.startup(Box::new(move |x| {
            manager_tx_clone
                .send(MsgToManager::QueryResponse(x))
                .unwrap()
        }));
        lsp_responder.startup(Box::new(move |_| {}));

        let query_system_actor = TaskManager::spawn_actor(query_system);
        let lsp_responder_actor = TaskManager::spawn_actor(lsp_responder);

        let task_manager = TaskManager {
            live_recipes: HashMap::new(),
            receive_channel: manager_rx,

            query_system: query_system_actor,
            lsp_responder: lsp_responder_actor,
        };

        let join_handle = thread::spawn(move || {
            task_manager.message_loop();
        });

        ActorControl {
            channel: manager_tx,
            join_handle,
        }
    }

    fn join_worker_threads(self) {
        let _ = self.query_system.join_handle.join();
        let _ = self.lsp_responder.join_handle.join();
    }

    fn send_next_step(&mut self, task_id: TaskId, argument: Box<dyn std::any::Any>) {
        match self.live_recipes.get_mut(&task_id) {
            Some(x) => {
                if x.len() > 0 {
                    let next_step = x.remove(0);

                    match next_step {
                        RecipeStep::GetTextForFile => {
                            if let Ok(location) = argument.downcast::<(Url, Position)>() {
                                self.query_system
                                    .channel
                                    .send(MsgFromManager::Message(QueryRequest::TypeAtPosition(
                                        task_id, location.0, location.1,
                                    )))
                                    .unwrap();
                            }
                        }
                        RecipeStep::RespondWithType => {
                            if let Ok(ty) = argument.downcast::<String>() {
                                self.lsp_responder
                                    .channel
                                    .send(MsgFromManager::Message(LspResponse::Type(task_id, *ty)))
                                    .unwrap();
                            } else {
                                panic!("Internal error: malformed RespondWithType");
                            }
                        }
                        RecipeStep::RespondWithInitialized => {
                            self.lsp_responder
                                .channel
                                .send(MsgFromManager::Message(LspResponse::Initialized(task_id)))
                                .unwrap();
                        }
                    }
                }
            }
            None => {
                //Do nothing as task has completed or it has been cancelled
            }
        }
    }

    fn do_recipe_for_lsp_request(&mut self, lsp_request: LspRequest) {
        match lsp_request {
            LspRequest::TypeForPos(task_id, url, position) => {
                let recipe = vec![RecipeStep::GetTextForFile, RecipeStep::RespondWithType];

                self.live_recipes.insert(task_id, recipe);
                self.send_next_step(task_id, Box::new((url, position)));
            }
            LspRequest::OpenFile(url, contents) => {
                self.query_system
                    .channel
                    .send(MsgFromManager::Message(QueryRequest::OpenFile(
                        url, contents,
                    )))
                    .unwrap();
            }
            LspRequest::Initialize(task_id) => {
                let recipe = vec![RecipeStep::RespondWithInitialized];

                self.live_recipes.insert(task_id, recipe);
                self.send_next_step(task_id, Box::new(()));
            }
        }
    }

    fn message_loop(mut self) {
        loop {
            match self.receive_channel.recv() {
                Ok(MsgToManager::QueryResponse(QueryResponse::Type(task_id, contents))) => {
                    self.send_next_step(task_id, Box::new(contents));
                }
                Ok(MsgToManager::LspRequest(lsp_request)) => {
                    self.do_recipe_for_lsp_request(lsp_request);
                }
                Ok(MsgToManager::Cancel(task_id)) => {
                    //Note: In the future we may have multiple steps to cancel a task
                    self.live_recipes.remove(&task_id);
                }
                Ok(MsgToManager::Shutdown) => {
                    let _ = self.lsp_responder.channel.send(MsgFromManager::Shutdown);
                    let _ = self.query_system.channel.send(MsgFromManager::Shutdown);
                    break;
                }
                Err(_) => {
                    eprintln!("Error during host receive");
                }
            }
        }

        self.join_worker_threads();
    }

    fn spawn_actor<T: Actor + Send + 'static>(
        mut actor: T,
    ) -> ActorControl<MsgFromManager<T::InMessage>> {
        let (actor_tx, actor_rx) = channel();

        let handle = thread::spawn(move || loop {
            match actor_rx.recv() {
                Ok(MsgFromManager::Message(message)) => actor.receive_message(message),
                Ok(MsgFromManager::Shutdown) => break,
                Err(_) => {
                    eprintln!("Failure during top-level message receive");
                    break;
                }
            }
        });

        ActorControl {
            channel: actor_tx,
            join_handle: handle,
        }
    }
}