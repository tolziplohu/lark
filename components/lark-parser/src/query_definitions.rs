use crate::ir::ParsedFile;
use crate::lexer::definition::LexerState;
use crate::lexer::token::LexToken;
use crate::lexer::tools::Tokenizer;
use crate::parser::Parser;
use crate::syntax::entity::{EntitySyntax, ParsedEntity};
use crate::syntax::skip_newline::SkipNewline;
use crate::ParserDatabase;

use debug::DebugWith;
use intern::{Intern, Untern};
use lark_entity::MemberKind;
use lark_entity::{Entity, EntityData};
use lark_error::ErrorReported;
use lark_error::WithError;
use lark_hir as hir;
use lark_seq::Seq;
use lark_span::{ByteIndex, FileName, Location, Span, Spanned};
use lark_string::GlobalIdentifier;
use std::sync::Arc;

crate fn file_tokens(
    db: &impl ParserDatabase,
    file_name: FileName,
) -> WithError<Seq<Spanned<LexToken, FileName>>> {
    let input = db.file_text(file_name);
    let mut tokenizer: Tokenizer<'_, LexerState> = Tokenizer::new(&input);
    let mut errors = vec![];
    let mut tokens = vec![];
    while let Some(token) = tokenizer.next() {
        match token {
            Ok(t) => tokens.push(t.in_file_named(file_name)),
            Err(span) => errors.push(crate::diagnostic(
                "unrecognized token",
                span.in_file_named(file_name),
            )),
        }
    }

    // Note: the EOF token is constructed "on the fly" by the parser
    // when the end of the current sequence of tokens is reached.

    WithError {
        value: Seq::from(tokens),
        errors,
    }
}

crate fn parsed_file(db: &impl ParserDatabase, file_name: FileName) -> WithError<ParsedFile> {
    log::debug!("root_entities({})", file_name.debug_with(db));

    parse_file(db, file_name)
        .map(|(entities, len)| ParsedFile::new(file_name, entities, Span::new(file_name, 0, len)))
}

fn parse_file(
    db: &impl ParserDatabase,
    file_name: FileName,
) -> WithError<(Seq<ParsedEntity>, usize)> {
    let file_entity = EntityData::InputFile { file: file_name.id }.intern(db);
    let entity_macro_definitions = crate::macro_definitions(&db, file_entity);
    let input = &db.file_text(file_name);
    let tokens = &db.file_tokens(file_name).into_value();
    let parser = Parser::new(file_name, db, &entity_macro_definitions, input, tokens, 0);
    parser
        .parse_until_eof(SkipNewline(EntitySyntax::new(file_entity)))
        .map(|entities| (entities, input.len()))
}

crate fn child_parsed_entities(
    db: &impl ParserDatabase,
    entity: Entity,
) -> WithError<Seq<ParsedEntity>> {
    log::debug!("child_parsed_entities({})", entity.debug_with(db));

    match entity.untern(db) {
        EntityData::InputFile { file } => {
            let file_name = FileName { id: file };
            parse_file(db, file_name).map(|(entities, _)| entities)
        }

        EntityData::ItemName { .. } => db
            .parsed_entity(entity)
            .thunk
            .parse_children(entity, db)
            .map(Seq::from),

        EntityData::Error { .. } | EntityData::MemberName { .. } | EntityData::LangItem(_) => {
            WithError::ok(Seq::default())
        }
    }
}

crate fn parsed_entity(db: &impl ParserDatabase, entity: Entity) -> ParsedEntity {
    match entity.untern(db) {
        EntityData::ItemName { base, .. } => {
            let siblings = db.child_parsed_entities(base).into_value();

            siblings
                .iter()
                .find(|p| p.entity == entity)
                .unwrap_or_else(|| {
                    panic!(
                        "parsed_entity({}): entity not found amongst its siblings `{:?}`",
                        entity.debug_with(db),
                        siblings.debug_with(db),
                    )
                })
                .clone()
        }

        EntityData::Error { .. }
        | EntityData::InputFile { .. }
        | EntityData::MemberName { .. }
        | EntityData::LangItem(_) => {
            panic!(
                "cannot compute: `parsed_entity({:?})`",
                entity.debug_with(db),
            );
        }
    }
}

crate fn child_entities(db: &impl ParserDatabase, entity: Entity) -> Seq<Entity> {
    db.child_parsed_entities(entity)
        .into_value()
        .iter()
        .map(|parsed_entity| parsed_entity.entity)
        .collect()
}

crate fn fn_body(db: &impl ParserDatabase, entity: Entity) -> WithError<Arc<hir::FnBody>> {
    db.parsed_entity(entity)
        .thunk
        .parse_fn_body(entity, db)
        .map(Arc::new)
}

crate fn entity_span(db: &impl ParserDatabase, entity: Entity) -> Span<FileName> {
    db.parsed_entity(entity).full_span.in_file_named(
        entity
            .input_file(db)
            .expect("Unexpected entity_span for LangItem or Error"),
    )
}

crate fn line_offsets(db: &impl ParserDatabase, id: FileName) -> Seq<usize> {
    let text: &str = &db.file_text(id);
    let mut accumulator = 0;
    text.lines()
        .map(|line_text| {
            let line_start = accumulator;
            accumulator += line_text.len();
            if text[accumulator..].starts_with("\r\n") {
                accumulator += 1;
            } else if text[accumulator..].starts_with("\n") {
                accumulator += 1;
            }
            line_start
        })
        .chain(std::iter::once(text.len()))
        .collect()
}

crate fn location(db: &impl ParserDatabase, id: FileName, index: ByteIndex) -> Location {
    let line_offsets = db.line_offsets(id);
    let line = match line_offsets.binary_search(&index.to_usize()) {
        Ok(index) | Err(index) => index,
    };

    let line_start = line_offsets[line];
    let text: &str = &db.file_text(id);

    // count utf-8 characters to find column
    let column = text[line_start..index.to_usize()].chars().count();

    Location::new(line, column, index)
}

crate fn byte_index(db: &impl ParserDatabase, id: FileName, line: u64, column: u64) -> ByteIndex {
    let line = line as usize;
    let column = column as usize;
    let line_offsets = db.line_offsets(id);
    let line_start = line_offsets[line];
    ByteIndex::from(line_start + column)
}

crate fn descendant_entities(db: &impl ParserDatabase, root: Entity) -> Seq<Entity> {
    let mut entities = vec![root];

    // Go over each thing added to entities and add any nested
    // entities.
    let mut index = 0;
    while let Some(&entity) = entities.get(index) {
        index += 1;
        entities.extend(db.child_entities(entity).iter());
    }

    Seq::from(entities)
}

crate fn members(
    _db: &impl ParserDatabase,
    _owner: Entity,
) -> Result<Seq<hir::Member>, ErrorReported> {
    unimplemented!()
    //let u = db.uhir_of_entity(owner);
    //match &u.value {
    //    uhir::Entity::Struct(s) => Ok(s
    //        .fields
    //        .iter()
    //        .map(|f| {
    //            let field_entity = EntityData::MemberName {
    //                base: owner,
    //                kind: hir::MemberKind::Field,
    //                id: *f.name,
    //            }
    //            .intern(db);
    //
    //            Member {
    //                name: *f.name,
    //                kind: hir::MemberKind::Field,
    //                entity: field_entity,
    //            }
    //        })
    //        .collect()),
    //
    //    uhir::Entity::Def(_) => panic!("asked for members of a function"),
    //}
}

crate fn member_entity(
    _db: &impl ParserDatabase,
    _owner: Entity,
    _kind: MemberKind,
    _name: GlobalIdentifier,
) -> Option<Entity> {
    unimplemented!()
    //match db.members(owner) {
    //    Err(report) => Some(Entity::error_sentinel(db, report)),
    //
    //    Ok(members) => members
    //        .iter()
    //        .filter_map(|member| {
    //            if member.kind == kind && member.name == name {
    //                Some(member.entity)
    //            } else {
    //                None
    //            }
    //        })
    //        .next(),
    //}
}
