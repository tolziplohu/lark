use crate::ty;
use crate::ty::intern::TyInterners;
use crate::ty::BaseData;
use crate::ty::InferVar;
use crate::typeck::TypeChecker;
use crate::unify::Inferable;
use generational_arena::Arena;

#[derive(Copy, Clone, Debug)]
pub(super) struct OpIndex {
    index: generational_arena::Index,
}

pub(super) trait BoxedTypeCheckerOp {
    fn execute(self: Box<Self>, typeck: &mut TypeChecker);
}

struct ClosureTypeCheckerOp<C> {
    closure: C,
}

impl<'hir, C> BoxedTypeCheckerOp for ClosureTypeCheckerOp<C>
where
    C: FnOnce(&mut TypeChecker),
{
    fn execute(self: Box<Self>, typeck: &mut TypeChecker) {
        (self.closure)(typeck)
    }
}

impl TypeChecker {
    /// Enqueues a closure to execute when any of the
    /// variables in `values` are unified.
    pub(super) fn enqueue_op(
        &mut self,
        values: impl IntoIterator<Item = impl Inferable<TyInterners>>,
        closure: impl FnOnce(&mut TypeChecker) + 'static,
    ) {
        let op: Box<dyn BoxedTypeCheckerOp> = Box::new(ClosureTypeCheckerOp { closure });
        let op_index = OpIndex {
            index: self.ops_arena.insert(op),
        };
        let mut inserted = false;
        for infer_value in values {
            // Check if `infer_value` represents an unbound inference variable.
            if let Err(var) = self.unify.shallow_resolve_data(infer_value) {
                // As yet unbound. Enqueue this op to be notified when
                // it does get bound.
                self.ops_blocked.entry(var).or_insert(vec![]).push(op_index);
                inserted = true;
            }
        }
        assert!(
            inserted,
            "enqueued an op with no unknown inference variables"
        );
    }

    /// Executes any closures that are blocked on `var`.
    pub(super) fn trigger_ops(&mut self, var: InferVar) {
        let blocked_ops = self.ops_blocked.remove(&var).unwrap_or(vec![]);
        for OpIndex { index } in blocked_ops {
            match self.ops_arena.remove(index) {
                None => {
                    // The op may already have been removed. This occurs
                    // when -- for example -- the same op is blocked on multiple variables.
                    // In that case, just ignore it.
                }

                Some(op) => {
                    op.execute(self);
                }
            }
        }
    }
}
