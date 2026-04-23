//! Proc-macro crate for embra tool registration.
//!
//! Apply `#[embra_tool(name = "...", description = "...")]` to an args struct
//! alongside `#[derive(serde::Deserialize, schemars::JsonSchema)]`. The macro
//! emits an `inventory::submit!` block that constructs a `ToolDescriptor` at
//! path `crate::tools::registry::ToolDescriptor` in the consuming crate, and
//! wires up a handler that deserializes the input and calls the args struct's
//! inherent `run(self, ctx) -> Result<String, DispatchError>` method.
//!
//! The consuming crate must have `inventory`, `schemars`, `serde_json`, and
//! `embra-tools-core` in its dependency tree.

use proc_macro::TokenStream;
use quote::quote;
use syn::{
    parse::{Parse, ParseStream},
    parse_macro_input, ItemStruct, LitStr, Result, Token,
};

#[proc_macro_attribute]
pub fn embra_tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as EmbraToolArgs);
    let input = parse_macro_input!(item as ItemStruct);
    let ident = &input.ident;
    let name = &args.name;
    let description = &args.description;

    let expanded = quote! {
        #input

        const _: () = {
            ::inventory::submit! {
                crate::tools::registry::ToolDescriptor {
                    name: #name,
                    description: #description,
                    input_schema: || {
                        ::serde_json::to_value(::schemars::schema_for!(#ident))
                            .expect("schema_for! must produce valid JSON")
                    },
                    handler: |input, ctx| ::std::boxed::Box::pin(async move {
                        let args: #ident = ::serde_json::from_value(input)
                            .map_err(|e| ::embra_tools_core::DispatchError::BadInput {
                                tool: ::std::string::String::from(#name),
                                source: e,
                            })?;
                        args.run(ctx).await
                    }),
                }
            }
        };
    };

    expanded.into()
}

struct EmbraToolArgs {
    name: LitStr,
    description: LitStr,
}

impl Parse for EmbraToolArgs {
    fn parse(input: ParseStream) -> Result<Self> {
        let mut name: Option<LitStr> = None;
        let mut description: Option<LitStr> = None;

        while !input.is_empty() {
            let key: syn::Ident = input.parse()?;
            let _eq: Token![=] = input.parse()?;
            let value: LitStr = input.parse()?;
            match key.to_string().as_str() {
                "name" => name = Some(value),
                "description" => description = Some(value),
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "unknown embra_tool key: `{other}` (expected `name` or `description`)"
                        ),
                    ));
                }
            }
            if input.is_empty() {
                break;
            }
            let _comma: Token![,] = input.parse()?;
        }

        let name = name.ok_or_else(|| {
            syn::Error::new(input.span(), "missing `name` in #[embra_tool(...)]")
        })?;
        let description = description.ok_or_else(|| {
            syn::Error::new(
                input.span(),
                "missing `description` in #[embra_tool(...)]",
            )
        })?;

        Ok(EmbraToolArgs { name, description })
    }
}
