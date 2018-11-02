use parser::prelude::*;

use ast::ast as a;
use crate as hir;
use crate::HirDatabase;
use lark_entity::Entity;
use lark_error::ErrorReported;
use lark_error::WithError;
use map::FxIndexMap;
use parser::pos::{Span, Spanned};
use parser::StringId;
use std::sync::Arc;

crate fn fn_body(db: &impl HirDatabase, item_id: Entity) -> WithError<Arc<crate::FnBody>> {
    let mut errors = vec![];
    let fn_body = HirLower::new(db, &mut errors).lower_ast_of_item(item_id);
    WithError {
        value: Arc::new(fn_body),
        errors,
    }
}

struct HirLower<'me, DB: HirDatabase> {
    db: &'me DB,
    fn_body_tables: hir::FnBodyTables,
    variables: FxIndexMap<StringId, hir::Variable>,
    errors: &'me mut Vec<Span>,
}

impl<'me, DB> HirLower<'me, DB>
where
    DB: HirDatabase,
{
    fn new(db: &'me DB, errors: &'me mut Vec<Span>) -> Self {
        HirLower {
            db,
            errors,
            fn_body_tables: Default::default(),
            variables: Default::default(),
        }
    }

    fn add<D: hir::HirIndexData>(&mut self, span: Span, node: D) -> D::Index {
        D::index_vec_mut(&mut self.fn_body_tables).push(Spanned(node, span))
    }

    fn span(&self, index: impl hir::SpanIndex) -> Span {
        index.span_from(&self.fn_body_tables)
    }

    fn save_scope(&self) -> FxIndexMap<StringId, hir::Variable> {
        self.variables.clone()
    }

    fn restore_scope(&mut self, scope: FxIndexMap<StringId, hir::Variable>) {
        self.variables = scope;
    }

    /// Brings a variable into scope, returning anything that was shadowed.
    fn bring_into_scope(&mut self, variable: hir::Variable) {
        let name = self[variable].name;
        self.variables.insert(self[name].text, variable);
    }

    fn lower_ast_of_item(mut self, item_id: Entity) -> hir::FnBody {
        match self.db.ast_of_item(item_id) {
            Ok(ast) => match &*ast {
                a::Item::Struct(_) => panic!("asked for fn-body of struct {:?}", item_id),
                a::Item::Def(def) => {
                    let arguments = self.lower_parameters(&def.parameters);

                    for &argument in &arguments {
                        self.bring_into_scope(argument);
                    }

                    let root_expression = self.lower_block(&def.body);

                    let arguments = hir::List::from_iterator(&mut self.fn_body_tables, arguments);

                    hir::FnBody {
                        arguments,
                        root_expression,
                        tables: self.fn_body_tables,
                    }
                }
            },

            Err(ErrorReported(ref spans)) => {
                let root_expression =
                    self.error_expression(*spans.first().unwrap(), hir::ErrorData::Misc);

                hir::FnBody {
                    arguments: hir::List::default(),
                    root_expression,
                    tables: self.fn_body_tables,
                }
            }
        }
    }

    fn lower_parameters(&mut self, parameters: &Vec<a::Field>) -> Vec<hir::Variable> {
        parameters
            .iter()
            .map(|parameter| {
                let name = self.add(
                    parameter.name.span(),
                    hir::IdentifierData {
                        text: *parameter.name,
                    },
                );
                self.add(parameter.span, hir::VariableData { name })
            })
            .collect()
    }

    fn lower_block(&mut self, block: &Spanned<a::Block>) -> hir::Expression {
        self.lower_block_items(&block.expressions)
            .unwrap_or_else(|| self.unit_expression(block.span()))
    }

    fn lower_block_items(&mut self, block_items: &[a::BlockItem]) -> Option<hir::Expression> {
        if block_items.is_empty() {
            return None;
        }

        match &block_items[0] {
            a::BlockItem::Item(_) => return self.lower_block_items(&block_items[1..]),

            a::BlockItem::Decl(decl) => match decl {
                a::Declaration::Let(l) => Some(self.lower_let(l, block_items)),
            },

            a::BlockItem::Expr(expr) => {
                let first = self.lower_expression(expr);

                match self.lower_block_items(&block_items[1..]) {
                    None => Some(first),

                    Some(second) => {
                        let span = self.span(second);
                        Some(self.add(span, hir::ExpressionData::Sequence { first, second }))
                    }
                }
            }
        }
    }

    fn lower_let(&mut self, let_decl: &a::Let, block_items: &[a::BlockItem]) -> hir::Expression {
        let saved_scope = self.save_scope();

        let a::Let {
            pattern,
            ty: _, /* FIXME */
            init,
        } = let_decl;

        let variable = match **pattern {
            a::Pattern::Underscore => unimplemented!("underscore patterns -- too lazy"),

            a::Pattern::Identifier(identifier, _mode) => {
                let name = self.add(identifier.span(), hir::IdentifierData { text: *identifier });
                self.add(identifier.span(), hir::VariableData { name })
            }
        };

        let variable_span = self.span(variable);

        let initializer = init
            .as_ref()
            .map(|expression| self.lower_expression(expression));

        self.bring_into_scope(variable);

        let body = self
            .lower_block_items(block_items)
            .unwrap_or_else(|| self.unit_expression(variable_span)); // FIXME: wrong span

        self.restore_scope(saved_scope);

        self.add(
            variable_span,
            hir::ExpressionData::Let {
                variable,
                initializer,
                body,
            },
        )
    }

    fn lower_expression(&mut self, expr: &a::Expression) -> hir::Expression {
        match expr {
            a::Expression::Block(block) => self.lower_block(block),

            a::Expression::Literal(..)
            | a::Expression::Interpolation(..)
            | a::Expression::Binary(..)
            | a::Expression::Call(_)
            | a::Expression::ConstructStruct(_) => self.unimplemented(expr.span()),

            a::Expression::Ref(_) => {
                let place = self.lower_place(expr);
                let span = self.span(place);
                let perm = self.add(span, hir::PermData::Default);
                self.add(span, hir::ExpressionData::Place { perm, place })
            }
        }
    }

    fn unimplemented(&mut self, span: Span) -> hir::Expression {
        self.errors.push(span);
        let error = self.add(span, hir::ErrorData::Unimplemented);
        self.add(span, hir::ExpressionData::Error { error })
    }

    fn lower_place(&mut self, expr: &a::Expression) -> hir::Place {
        match expr {
            a::Expression::Ref(identifier) => match self.variables.get(identifier.node()) {
                Some(&variable) => self.add(identifier.span(), hir::PlaceData::Variable(variable)),

                None => {
                    let error_expression = self.error_expression(
                        identifier.span(),
                        hir::ErrorData::UnknownIdentifier {
                            text: *identifier.node(),
                        },
                    );

                    self.add(
                        identifier.span(),
                        hir::PlaceData::Temporary(error_expression),
                    )
                }
            },

            a::Expression::Block(_)
            | a::Expression::ConstructStruct(_)
            | a::Expression::Call(_)
            | a::Expression::Binary(..)
            | a::Expression::Interpolation(..)
            | a::Expression::Literal(..) => {
                let expression = self.lower_expression(expr);
                let span = self.span(expression);
                self.add(span, hir::PlaceData::Temporary(expression))
            }
        }
    }

    fn error_expression(&mut self, span: Span, data: hir::ErrorData) -> hir::Expression {
        let error = self.add(span, data);
        self.add(span, hir::ExpressionData::Error { error })
    }

    fn unit_expression(&mut self, span: Span) -> hir::Expression {
        self.add(span, hir::ExpressionData::Unit {})
    }
}

impl<'me, DB, I> std::ops::Index<I> for HirLower<'me, DB>
where
    DB: HirDatabase,
    I: hir::HirIndex,
{
    type Output = I::Data;

    fn index(&self, index: I) -> &I::Data {
        &self.fn_body_tables[index]
    }
}
