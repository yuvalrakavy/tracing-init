//! Attribute used by tracing-init's optional nextest instrumentation.
use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, ItemFn};

/// Replace `#[test]` or `#[tokio::test(...)]` to capture one nextest attempt.
///
/// If `LOGMON_TEST_TRACE_ID` is absent, the test runs with its original harness
/// semantics and can install its own subscriber.
#[proc_macro_attribute]
pub fn logmon_test(options: TokenStream, body: TokenStream) -> TokenStream {
    let mut func = parse_macro_input!(body as ItemFn);
    let name = &func.sig.ident;
    let block = &func.block;
    let is_async = func.sig.asyncness.is_some();
    let existing_harness = func.attrs.iter().any(|attr| {
        attr.path()
            .segments
            .last()
            .is_some_and(|part| part.ident == "test")
    });
    if !options.is_empty() && (!is_async || existing_harness) {
        return syn::Error::new_spanned(
            name,
            "Tokio test options require an async function without a second test attribute",
        )
        .into_compile_error()
        .into();
    }
    let runtime = quote!(::tracing_init::logmon_test_runtime);
    let wrapped = if is_async {
        quote!({
            let __logmon = #runtime::begin(concat!(module_path!(), "::", stringify!(#name)));
            #runtime::instrument(async move #block, &__logmon).await
        })
    } else {
        quote!({
            let __logmon = #runtime::begin(concat!(module_path!(), "::", stringify!(#name)));
            let __entered = __logmon.as_ref().map(|attempt| attempt.span.enter());
            #block
        })
    };
    func.block = Box::new(syn::parse2(wrapped).expect("valid instrumented block"));
    let harness = if existing_harness {
        quote!()
    } else if is_async {
        let options: proc_macro2::TokenStream = options.into();
        quote!(#[::tokio::test(#options)])
    } else {
        quote!(#[::core::prelude::v1::test])
    };
    quote!(#harness #func).into()
}
