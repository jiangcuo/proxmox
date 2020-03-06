use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};

use failure::Error;

use proc_macro2::{Ident, Span, TokenStream};
use quote::quote_spanned;

use super::Schema;
use crate::api;
use crate::serde;
use crate::util::{self, FieldName, JSONObject};

pub fn handle_struct(attribs: JSONObject, stru: syn::ItemStruct) -> Result<TokenStream, Error> {
    match &stru.fields {
        // unit structs, not sure about these?
        syn::Fields::Unit => handle_unit_struct(attribs, stru),
        syn::Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
            handle_newtype_struct(attribs, stru)
        }
        syn::Fields::Unnamed(fields) => bail!(
            fields.paren_token.span,
            "api macro does not support tuple structs"
        ),
        syn::Fields::Named(_) => handle_regular_struct(attribs, stru)
    }
}

fn get_struct_description(schema: &mut Schema, stru: &syn::ItemStruct) -> Result<(), Error> {
    if schema.description.is_none() {
        let (doc_comment, doc_span) = util::get_doc_comments(&stru.attrs)?;
        util::derive_descriptions(schema, &mut None, &doc_comment, doc_span)?;
    }

    Ok(())
}

fn handle_unit_struct(
    attribs: JSONObject,
    stru: syn::ItemStruct,
) -> Result<TokenStream, Error> {
    // unit structs, not sure about these?

    let mut schema: Schema = if attribs.is_empty() {
        Schema::empty_object(Span::call_site())
    } else {
        attribs.try_into()?
    };

    get_struct_description(&mut schema, &stru)?;

    finish_schema(schema, &stru, &stru.ident)
}

fn finish_schema(
    schema: Schema,
    stru: &syn::ItemStruct,
    name: &Ident,
) -> Result<TokenStream, Error> {
    let schema = {
        let mut ts = TokenStream::new();
        schema.to_schema(&mut ts)?;
        ts
    };

    Ok(quote_spanned! { name.span() =>
        #stru
        impl #name {
            pub const API_SCHEMA: &'static ::proxmox::api::schema::Schema = #schema;
        }
    })
}

fn handle_newtype_struct(
    attribs: JSONObject,
    stru: syn::ItemStruct,
) -> Result<TokenStream, Error> {
    let mut schema: Schema = if attribs.is_empty() {
        Schema::empty_object(Span::call_site())
    } else {
        attribs.try_into()?
    };

    get_struct_description(&mut schema, &stru)?;

    finish_schema(schema, &stru, &stru.ident)
}

fn handle_regular_struct(
    attribs: JSONObject,
    stru: syn::ItemStruct,
) -> Result<TokenStream, Error> {
    let mut schema: Schema = if attribs.is_empty() {
        Schema::empty_object(Span::call_site())
    } else {
        attribs.try_into()?
    };

    get_struct_description(&mut schema, &stru)?;

    // sanity check, first get us some quick by-name access to our fields:
    //
    // NOTE: We remove references we're "done with" and in the end fail with a list of extraneous
    // fields if there are any.
    let mut schema_fields: HashMap<String, &mut (FieldName, bool, Schema)> = HashMap::new();

    // We also keep a reference to the SchemaObject around since we derive missing fields
    // automatically.
    if let api::SchemaItem::Object(ref mut obj) = &mut schema.item {
        for field in obj.properties_mut() {
            schema_fields.insert(field.0.as_str().to_string(), field);
        }
    } else {
        bail!(schema.span, "structs need an object schema");
    }

    let mut new_fields: Vec<(FieldName, bool, Schema)> = Vec::new();

    let container_attrs = serde::ContainerAttrib::try_from(&stru.attrs[..])?;

    if let syn::Fields::Named(ref fields) = &stru.fields {
        for field in &fields.named {
            let attrs = serde::SerdeAttrib::try_from(&field.attrs[..])?;

            let (name, span) = {
                let ident: &Ident = field
                    .ident
                    .as_ref()
                    .ok_or_else(|| format_err!(field => "field without name?"))?;

                if let Some(renamed) = attrs.rename {
                    (renamed.into_str(), ident.span())
                } else if let Some(rename_all) = container_attrs.rename_all {
                    let name = rename_all.apply_to_field(&ident.to_string());
                    (name, ident.span())
                } else {
                    (ident.to_string(), ident.span())
                }
            };

            match schema_fields.remove(&name) {
                Some(field_def) => handle_regular_field(field_def, field, false)?,
                None => {
                    let mut field_def = (
                        FieldName::new(name.clone(), span),
                        false,
                        Schema::blank(span),
                    );
                    handle_regular_field(&mut field_def, field, true)?;
                    new_fields.push(field_def);
                }
            }
        }
    } else {
        unreachable!();
    };

    // now error out about all the fields not found in the struct:
    if !schema_fields.is_empty() {
        let bad_fields = schema_fields.keys().fold(String::new(), |mut acc, key| {
            if !acc.is_empty() {
                acc.push_str(", ");
                acc
            } else {
                key.to_owned()
            }
        });
        bail!(
            schema.span,
            "struct does not contain the following fields: {}",
            bad_fields
        );
    }

    // add the fields we derived:
    if let api::SchemaItem::Object(ref mut obj) = &mut schema.item {
        obj.extend_properties(new_fields);
    } else {
        unreachable!();
    }

    finish_schema(schema, &stru, &stru.ident)
}

/// Field handling:
///
/// For each field we derive the description from doc-attributes if available.
fn handle_regular_field(
    field_def: &mut (FieldName, bool, Schema),
    field: &syn::Field,
    derived: bool, // whether this field was missing in the schema
) -> Result<(), Error> {
    let schema: &mut Schema = &mut field_def.2;

    if schema.description.is_none() {
        let (doc_comment, doc_span) = util::get_doc_comments(&field.attrs)?;
        util::derive_descriptions(schema, &mut None, &doc_comment, doc_span)?;
    }

    util::infer_type(schema, &field.ty)?;

    if is_option_type(&field.ty) {
        if derived {
            field_def.1 = true;
        } else if !field_def.1 {
            bail!(&field.ty => "non-optional Option type?");
        }
    }

    Ok(())
}

/// Note that we cannot handle renamed imports at all here...
fn is_option_type(ty: &syn::Type) -> bool {
    if let syn::Type::Path(p) = ty {
        if p.qself.is_some() {
            return false;
        }
        let segs = &p.path.segments;
        match segs.len() {
            1 => return segs.last().unwrap().ident == "Option",
            2 => {
                return segs.first().unwrap().ident == "std"
                    && segs.last().unwrap().ident == "Option"
            }
            _ => return false,
        }
    }
    false
}
