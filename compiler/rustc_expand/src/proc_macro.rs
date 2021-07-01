use crate::base::{self, *};
use crate::proc_macro_server;

use rustc_ast as ast;
use rustc_ast::ptr::P;
use rustc_ast::token;
use rustc_ast::tokenstream::{CanSynthesizeMissingTokens, TokenStream, TokenTree};
use rustc_data_structures::sync::Lrc;
use rustc_errors::ErrorReported;
use rustc_parse::nt_to_tokenstream;
use rustc_parse::parser::ForceCollect;
use rustc_span::def_id::CrateNum;
use rustc_span::{Span, DUMMY_SP};

struct CrossbeamMessagePipe<T> {
    tx: crossbeam_channel::Sender<T>,
    rx: crossbeam_channel::Receiver<T>,
}

impl<T> pm::bridge::server::MessagePipe<T> for CrossbeamMessagePipe<T> {
    fn new() -> (Self, Self) {
        let (tx1, rx1) = crossbeam_channel::unbounded();
        let (tx2, rx2) = crossbeam_channel::unbounded();
        (CrossbeamMessagePipe { tx: tx1, rx: rx2 }, CrossbeamMessagePipe { tx: tx2, rx: rx1 })
    }

    fn send(&mut self, value: T) {
        self.tx.send(value).unwrap();
    }

    fn recv(&mut self) -> Option<T> {
        self.rx.recv().ok()
    }
}

fn exec_strategy(ecx: &ExtCtxt<'_>) -> impl pm::bridge::server::ExecutionStrategy {
    <pm::bridge::server::MaybeCrossThread<CrossbeamMessagePipe<_>>>::new(
        ecx.sess.opts.debugging_opts.proc_macro_cross_thread,
    )
}

pub struct BangProcMacro {
    pub client: pm::bridge::client::Client<fn(pm::TokenStream) -> pm::TokenStream>,
    pub krate: CrateNum,
}

impl base::ProcMacro for BangProcMacro {
    fn expand<'cx>(
        &self,
        ecx: &'cx mut ExtCtxt<'_>,
        span: Span,
        input: TokenStream,
    ) -> Result<TokenStream, ErrorReported> {
        let server = proc_macro_server::Rustc::new(ecx, self.krate);
        self.client.run(&exec_strategy(ecx), server, input, ecx.ecfg.proc_macro_backtrace).map_err(
            |e| {
                let mut err = ecx.struct_span_err(span, "proc macro panicked");
                if let Some(s) = e.as_str() {
                    err.help(&format!("message: {}", s));
                }
                err.emit();
                ErrorReported
            },
        )
    }
}

pub struct AttrProcMacro {
    pub client: pm::bridge::client::Client<fn(pm::TokenStream, pm::TokenStream) -> pm::TokenStream>,
    pub krate: CrateNum,
}

impl base::AttrProcMacro for AttrProcMacro {
    fn expand<'cx>(
        &self,
        ecx: &'cx mut ExtCtxt<'_>,
        span: Span,
        annotation: TokenStream,
        annotated: TokenStream,
    ) -> Result<TokenStream, ErrorReported> {
        let server = proc_macro_server::Rustc::new(ecx, self.krate);
        self.client
            .run(&exec_strategy(ecx), server, annotation, annotated, ecx.ecfg.proc_macro_backtrace)
            .map_err(|e| {
                let mut err = ecx.struct_span_err(span, "custom attribute panicked");
                if let Some(s) = e.as_str() {
                    err.help(&format!("message: {}", s));
                }
                err.emit();
                ErrorReported
            })
    }
}

pub struct ProcMacroDerive {
    pub client: pm::bridge::client::Client<fn(pm::TokenStream) -> pm::TokenStream>,
    pub krate: CrateNum,
}

impl MultiItemModifier for ProcMacroDerive {
    fn expand(
        &self,
        ecx: &mut ExtCtxt<'_>,
        span: Span,
        _meta_item: &ast::MetaItem,
        item: Annotatable,
    ) -> ExpandResult<Vec<Annotatable>, Annotatable> {
        // We need special handling for statement items
        // (e.g. `fn foo() { #[derive(Debug)] struct Bar; }`)
        let mut is_stmt = false;
        let item = match item {
            Annotatable::Item(item) => token::NtItem(item),
            Annotatable::Stmt(stmt) => {
                is_stmt = true;
                assert!(stmt.is_item());

                // A proc macro can't observe the fact that we're passing
                // them an `NtStmt` - it can only see the underlying tokens
                // of the wrapped item
                token::NtStmt(stmt.into_inner())
            }
            _ => unreachable!(),
        };
        let input = if crate::base::pretty_printing_compatibility_hack(&item, &ecx.sess.parse_sess)
        {
            TokenTree::token(token::Interpolated(Lrc::new(item)), DUMMY_SP).into()
        } else {
            nt_to_tokenstream(&item, &ecx.sess.parse_sess, CanSynthesizeMissingTokens::No)
        };

        let server = proc_macro_server::Rustc::new(ecx, self.krate);
        let stream = match self.client.run(
            &exec_strategy(ecx),
            server,
            input,
            ecx.ecfg.proc_macro_backtrace,
        ) {
            Ok(stream) => stream,
            Err(e) => {
                let mut err = ecx.struct_span_err(span, "proc-macro derive panicked");
                if let Some(s) = e.as_str() {
                    err.help(&format!("message: {}", s));
                }
                err.emit();
                return ExpandResult::Ready(vec![]);
            }
        };

        let error_count_before = ecx.sess.parse_sess.span_diagnostic.err_count();
        let mut parser =
            rustc_parse::stream_to_parser(&ecx.sess.parse_sess, stream, Some("proc-macro derive"));
        let mut items = vec![];

        loop {
            match parser.parse_item(ForceCollect::No) {
                Ok(None) => break,
                Ok(Some(item)) => {
                    if is_stmt {
                        items.push(Annotatable::Stmt(P(ecx.stmt_item(span, item))));
                    } else {
                        items.push(Annotatable::Item(item));
                    }
                }
                Err(mut err) => {
                    err.emit();
                    break;
                }
            }
        }

        // fail if there have been errors emitted
        if ecx.sess.parse_sess.span_diagnostic.err_count() > error_count_before {
            ecx.struct_span_err(span, "proc-macro derive produced unparseable tokens").emit();
        }

        ExpandResult::Ready(items)
    }
}
