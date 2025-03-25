use proc_macro::TokenStream;
use darling::FromDeriveInput;
use quote::quote;

#[proc_macro_derive(Command)]
pub fn derive_command(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as syn::DeriveInput);
    let input_name = &input.ident;
    
    let token = quote! {
        impl ::nitinol::Command for #input_name {}
    };
    
    token.into()
}

#[derive(FromDeriveInput)]
#[darling(attributes(persist))]
struct PersistAttribute {
    #[darling(default)]
    key: String,
    enc: String,
    dec: String
}

#[proc_macro_derive(Event, attributes(persist))]
pub fn derive_event(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as syn::DeriveInput);
    let attr = match PersistAttribute::from_derive_input(&input) {
        Ok(v) => v,
        Err(e) => return e.write_errors().into()
    };
    let input_name = &input.ident;
    
    let event_type = if attr.key.is_empty() {
        to_kebab_case(&input_name.to_string())
    } else {
        attr.key
    };
    
    let enc = syn::parse_str::<syn::Expr>(&attr.enc).unwrap();
    let dec = syn::parse_str::<syn::Expr>(&attr.dec).unwrap();
    
    let token = quote! {
        impl ::nitinol::Event for #input_name {
            const EVENT_TYPE: &'static str = #event_type;
            fn as_bytes(&self) -> Result<Vec<u8>, ::nitinol::errors::SerializeError> {
                Ok(#enc(self)?)
            }
            fn from_bytes(bytes: &[u8]) -> Result<Self, ::nitinol::errors::DeserializeError> {
                Ok(#dec(bytes)?)
            }
        }
    };
    
    token.into()
}

fn to_kebab_case(input: &str) -> String {
    input.chars().fold(String::new(), |mut acc, c| {
        if c.is_uppercase() {
            if !acc.is_empty() {
                acc.push('-');
            }
            acc.push(c.to_ascii_lowercase());
        } else {
            acc.push(c);
        }
        acc
    })
}
