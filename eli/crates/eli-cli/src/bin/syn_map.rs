use std::env;
use std::fs;
use std::path::Path;

use proc_macro2::TokenStream;
use quote::ToTokens;
use serde::Serialize;
use syn::visit::Visit;
use syn::{Expr, ImplItem, Item};

#[derive(Default)]
struct ComplexityVisitor {
    expr_total: usize,
    expr_if: usize,
    expr_match: usize,
    expr_loop: usize,
    expr_call: usize,
    expr_method_call: usize,
}

impl<'ast> Visit<'ast> for ComplexityVisitor {
    fn visit_expr(&mut self, node: &'ast Expr) {
        self.expr_total += 1;
        match node {
            Expr::If(_) => self.expr_if += 1,
            Expr::Match(_) => self.expr_match += 1,
            Expr::ForLoop(_) | Expr::While(_) | Expr::Loop(_) => self.expr_loop += 1,
            Expr::Call(_) => self.expr_call += 1,
            Expr::MethodCall(_) => self.expr_method_call += 1,
            _ => {}
        }
        syn::visit::visit_expr(self, node);
    }
}

#[derive(Serialize)]
struct FnShape {
    name: String,
    async_fn: bool,
    statements: usize,
    token_len: usize,
    expr_total: usize,
    expr_if: usize,
    expr_match: usize,
    expr_loop: usize,
    expr_call: usize,
    expr_method_call: usize,
}

#[derive(Serialize)]
struct FileShape {
    path: String,
    lines: usize,
    top_level_items: usize,
    top_level_mods: usize,
    top_level_fns: usize,
    top_level_structs: usize,
    top_level_enums: usize,
    top_level_impls: usize,
    impl_methods: usize,
    functions: Vec<FnShape>,
}

fn token_len<T: ToTokens>(node: &T) -> usize {
    let mut ts = TokenStream::new();
    node.to_tokens(&mut ts);
    ts.to_string().len()
}

fn function_shape(name: String, async_fn: bool, block: &syn::Block) -> FnShape {
    let mut visitor = ComplexityVisitor::default();
    visitor.visit_block(block);
    FnShape {
        name,
        async_fn,
        statements: block.stmts.len(),
        token_len: token_len(block),
        expr_total: visitor.expr_total,
        expr_if: visitor.expr_if,
        expr_match: visitor.expr_match,
        expr_loop: visitor.expr_loop,
        expr_call: visitor.expr_call,
        expr_method_call: visitor.expr_method_call,
    }
}

fn analyze_file(path: &Path) -> anyhow::Result<FileShape> {
    let src = fs::read_to_string(path)?;
    let ast = syn::parse_file(&src)?;

    let mut top_level_mods = 0usize;
    let mut top_level_fns = 0usize;
    let mut top_level_structs = 0usize;
    let mut top_level_enums = 0usize;
    let mut top_level_impls = 0usize;
    let mut impl_methods = 0usize;
    let mut functions = Vec::new();

    for item in &ast.items {
        match item {
            Item::Mod(_) => top_level_mods += 1,
            Item::Fn(f) => {
                top_level_fns += 1;
                functions.push(function_shape(
                    f.sig.ident.to_string(),
                    f.sig.asyncness.is_some(),
                    &f.block,
                ));
            }
            Item::Struct(_) => top_level_structs += 1,
            Item::Enum(_) => top_level_enums += 1,
            Item::Impl(imp) => {
                top_level_impls += 1;
                for ii in &imp.items {
                    if let ImplItem::Fn(m) = ii {
                        impl_methods += 1;
                        let ty_name = imp.self_ty.to_token_stream().to_string();
                        let method_name = format!("{}::{}", ty_name, m.sig.ident);
                        functions.push(function_shape(
                            method_name,
                            m.sig.asyncness.is_some(),
                            &m.block,
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    functions.sort_by(|a, b| b.token_len.cmp(&a.token_len));
    functions.truncate(20);

    Ok(FileShape {
        path: path.display().to_string(),
        lines: src.lines().count(),
        top_level_items: ast.items.len(),
        top_level_mods,
        top_level_fns,
        top_level_structs,
        top_level_enums,
        top_level_impls,
        impl_methods,
        functions,
    })
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        anyhow::bail!("usage: syn_map <file.rs> [more.rs...]");
    }
    let mut out = Vec::new();
    for p in args {
        out.push(analyze_file(Path::new(&p))?);
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
