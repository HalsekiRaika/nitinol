use proc_macro::TokenStream;
use quote::quote;

#[proc_macro_derive(Command)]
pub fn derive_command(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as syn::DeriveInput);
    let input_name = &input.ident;
    
    let token = quote! {
        impl nitinol_core::command::Command for #input_name {}
    };
    
    token.into()
}

#[proc_macro_derive(Event)]
pub fn derive_event(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as syn::DeriveInput);
    let input_name = &input.ident;
    
    let token = quote! {
        impl nitinol_core::event::Event for #input_name {
            const REGISTRY_KEY: &'static str = #input_name;
            fn as_bytes(&self) -> Result<Vec<u8>, SerializeError> {
                
            }
            fn from_bytes(bytes: &[u8]) -> Result<Self, DeserializeError> {
                
            }
        }
    };
    
    token.into()
}