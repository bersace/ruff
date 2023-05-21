use std::fmt;

use rustpython_parser::ast::{self, Constant, Expr, Ranged};

use ruff_diagnostics::{AlwaysAutofixableViolation, Diagnostic, Edit, Fix};
use ruff_macros::{derive_message_formats, violation};
use ruff_python_ast::str::is_implicit_concatenation;

use crate::checkers::ast::traits::Analyzer;
use crate::checkers::ast::RuleContext;
use crate::registry::{AsRule, Rule};

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub(crate) enum LiteralType {
    Str,
    Bytes,
}

impl fmt::Display for LiteralType {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            LiteralType::Str => fmt.write_str("str"),
            LiteralType::Bytes => fmt.write_str("bytes"),
        }
    }
}

#[violation]
pub struct NativeLiterals {
    literal_type: LiteralType,
}

impl AlwaysAutofixableViolation for NativeLiterals {
    #[derive_message_formats]
    fn message(&self) -> String {
        let NativeLiterals { literal_type } = self;
        format!("Unnecessary call to `{literal_type}`")
    }

    fn autofix_title(&self) -> String {
        let NativeLiterals { literal_type } = self;
        format!("Replace with `{literal_type}`")
    }
}

impl Analyzer<ast::ExprCall> for NativeLiterals {
    fn rule() -> Rule {
        Rule::NativeLiterals
    }

    fn run(diagnostics: &mut Vec<Diagnostic>, checker: &RuleContext, node: &ast::ExprCall) {
        native_literals(diagnostics, checker, node);
    }
}

/// UP018
pub(crate) fn native_literals(
    diagnostics: &mut Vec<Diagnostic>,
    checker: &RuleContext,
    ast::ExprCall {
        func,
        args,
        keywords,
        range,
    }: &ast::ExprCall,
) {
    let Expr::Name(ast::ExprName { id, .. }) = func.as_ref() else {
        return;
    };

    if !keywords.is_empty() || args.len() > 1 {
        return;
    }

    if (id == "str" || id == "bytes") && checker.ctx.is_builtin(id) {
        let Some(arg) = args.get(0) else {
            let mut diagnostic = Diagnostic::new(NativeLiterals{literal_type:if id == "str" {
                LiteralType::Str
            } else {
                LiteralType::Bytes
            }}, *range);
            if checker.patch(diagnostic.kind.rule()) {
                let constant = if id == "bytes" {
                    Constant::Bytes(vec![])
                } else {
                    Constant::Str(String::new())
                };
                let content = checker.generator().constant(&constant);
                #[allow(deprecated)]
                diagnostic.set_fix(Fix::unspecified(Edit::range_replacement(
                    content,
                    *range,
                )));
            }
            diagnostics.push(diagnostic);
            return;
        };

        // Look for `str("")`.
        if id == "str"
            && !matches!(
                &arg,
                Expr::Constant(ast::ExprConstant {
                    value: Constant::Str(_),
                    ..
                }),
            )
        {
            return;
        }

        // Look for `bytes(b"")`
        if id == "bytes"
            && !matches!(
                &arg,
                Expr::Constant(ast::ExprConstant {
                    value: Constant::Bytes(_),
                    ..
                }),
            )
        {
            return;
        }

        // Skip implicit string concatenations.
        let arg_code = checker.locator.slice(arg.range());
        if is_implicit_concatenation(arg_code) {
            return;
        }

        let mut diagnostic = Diagnostic::new(
            NativeLiterals {
                literal_type: if id == "str" {
                    LiteralType::Str
                } else {
                    LiteralType::Bytes
                },
            },
            *range,
        );
        if checker.patch(diagnostic.kind.rule()) {
            #[allow(deprecated)]
            diagnostic.set_fix(Fix::unspecified(Edit::range_replacement(
                arg_code.to_string(),
                *range,
            )));
        }
        diagnostics.push(diagnostic);
    }
}
