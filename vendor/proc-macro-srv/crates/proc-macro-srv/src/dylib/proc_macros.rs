//! Proc macro ABI
use crate::{
    ProcMacroClientHandle, ProcMacroKind, ProcMacroSrvSpan, TrackedEnv, token_stream::TokenStream,
};
use rustc_proc_macro::bridge;

impl From<bridge::PanicMessage> for crate::PanicMessage {
    fn from(p: bridge::PanicMessage) -> Self {
        Self { message: p.into_string() }
    }
}

pub(crate) struct ProcMacros(Vec<(bridge::client::Client, ProcMacroKind, &'static str)>);

impl ProcMacros {
    pub(super) fn new(macros: Vec<(bridge::client::Client, ProcMacroKind, &'static str)>) -> Self {
        ProcMacros(macros)
    }

    pub(crate) fn expand<'a, S: ProcMacroSrvSpan>(
        &self,
        macro_name: &str,
        macro_body: TokenStream<S>,
        attribute: Option<TokenStream<S>>,
        def_site: S,
        call_site: S,
        mixed_site: S,
        tracked_env: &'a mut TrackedEnv,
        callback: Option<ProcMacroClientHandle<'a>>,
    ) -> Result<TokenStream<S>, crate::PanicMessage> {
        let parsed_attributes = attribute.unwrap_or_default();

        for (client, kind, name) in &self.0 {
            if *name != macro_name {
                continue;
            }
            match kind {
                ProcMacroKind::CustomDerive | ProcMacroKind::Bang => {
                    let res = client.run1(
                        &bridge::server::SAME_THREAD,
                        S::make_server(call_site, def_site, mixed_site, tracked_env, callback),
                        macro_body,
                        cfg!(debug_assertions),
                    );
                    return res.map_err(crate::PanicMessage::from);
                }
                ProcMacroKind::Attr => {
                    let res = client.run2(
                        &bridge::server::SAME_THREAD,
                        S::make_server(call_site, def_site, mixed_site, tracked_env, callback),
                        parsed_attributes,
                        macro_body,
                        cfg!(debug_assertions),
                    );
                    return res.map_err(crate::PanicMessage::from);
                }
            }
        }

        Err(bridge::PanicMessage::String(format!("proc-macro `{macro_name}` is missing")).into())
    }

    pub(crate) fn list_macros(&self) -> impl Iterator<Item = (&str, ProcMacroKind)> {
        self.0.iter().map(|(_client, kind, name)| (*name, *kind))
    }
}
