// Copyright (c) 2017-present PyO3 Project and Contributors
//! Code generation for the function that initializes a python module and adds classes and function.

use crate::method;
use crate::pyfunction;
use crate::pyfunction::PyFunctionAttr;
use crate::pymethod;
use crate::pymethod::get_arg_names;
use crate::utils;
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::Ident;

/// Generates the function that is called by the python interpreter to initialize the native
/// module
pub fn py_init(fnname: &Ident, name: &Ident, doc: syn::LitStr) -> TokenStream {
    let cb_name = Ident::new(&format!("PyInit_{}", name), Span::call_site());

    quote! {
        #[no_mangle]
        #[allow(non_snake_case)]
        /// This autogenerated function is called by the python interpreter when importing
        /// the module.
        pub unsafe extern "C" fn #cb_name() -> *mut pyo3::ffi::PyObject {
            use pyo3::derive_utils::ModuleDef;
            const NAME: &'static str = concat!(stringify!(#name), "\0");
            static MODULE_DEF: ModuleDef = unsafe { ModuleDef::new(NAME) };

            let pool = pyo3::GILPool::new();
            let py = pool.python();
            pyo3::callback_body!(_py, { MODULE_DEF.make_module(#doc, #fnname) })
        }
    }
}

/// Finds and takes care of the #[pyfn(...)] in `#[pymodule]`
pub fn process_functions_in_module(func: &mut syn::ItemFn) -> syn::Result<()> {
    let mut stmts: Vec<syn::Stmt> = Vec::new();

    for stmt in func.block.stmts.iter_mut() {
        if let syn::Stmt::Item(syn::Item::Fn(ref mut func)) = stmt {
            if let Some((module_name, python_name, pyfn_attrs)) =
                extract_pyfn_attrs(&mut func.attrs)
            {
                let function_to_python = add_fn_to_module(func, python_name, pyfn_attrs)?;
                let function_wrapper_ident = function_wrapper_ident(&func.sig.ident);
                let item: syn::ItemFn = syn::parse_quote! {
                    fn block_wrapper() {
                        #function_to_python
                        #module_name.add_wrapped(&#function_wrapper_ident)?;
                    }
                };
                stmts.extend(item.block.stmts.into_iter());
            }
        };
        stmts.push(stmt.clone());
    }

    func.block.stmts = stmts;
    Ok(())
}

/// Transforms a rust fn arg parsed with syn into a method::FnArg
fn wrap_fn_argument<'a>(cap: &'a syn::PatType, name: &'a Ident) -> syn::Result<method::FnArg<'a>> {
    let (mutability, by_ref, ident) = match *cap.pat {
        syn::Pat::Ident(ref patid) => (&patid.mutability, &patid.by_ref, &patid.ident),
        _ => return Err(syn::Error::new_spanned(&cap.pat, "Unsupported argument")),
    };

    let py = crate::utils::if_type_is_python(&cap.ty);
    let opt = method::check_ty_optional(&cap.ty);
    Ok(method::FnArg {
        name: ident,
        mutability,
        by_ref,
        ty: &cap.ty,
        optional: opt,
        py,
        reference: method::is_ref(&name, &cap.ty),
    })
}

/// Extracts the data from the #[pyfn(...)] attribute of a function
fn extract_pyfn_attrs(
    attrs: &mut Vec<syn::Attribute>,
) -> Option<(syn::Path, Ident, Vec<pyfunction::Argument>)> {
    let mut new_attrs = Vec::new();
    let mut fnname = None;
    let mut modname = None;
    let mut fn_attrs = Vec::new();

    for attr in attrs.iter() {
        match attr.parse_meta() {
            Ok(syn::Meta::List(ref list)) if list.path.is_ident("pyfn") => {
                let meta: Vec<_> = list.nested.iter().cloned().collect();
                if meta.len() >= 2 {
                    // read module name
                    match meta[0] {
                        syn::NestedMeta::Meta(syn::Meta::Path(ref path)) => {
                            modname = Some(path.clone())
                        }
                        _ => panic!("The first parameter of pyfn must be a MetaItem"),
                    }
                    // read Python function name
                    match meta[1] {
                        syn::NestedMeta::Lit(syn::Lit::Str(ref lits)) => {
                            fnname = Some(syn::Ident::new(&lits.value(), lits.span()));
                        }
                        _ => panic!("The second parameter of pyfn must be a Literal"),
                    }
                    // Read additional arguments
                    if list.nested.len() >= 3 {
                        fn_attrs = PyFunctionAttr::from_meta(&meta[2..meta.len()])
                            .unwrap()
                            .arguments;
                    }
                } else {
                    panic!("can not parse 'pyfn' params {:?}", attr);
                }
            }
            _ => new_attrs.push(attr.clone()),
        }
    }

    *attrs = new_attrs;
    Some((modname?, fnname?, fn_attrs))
}

/// Coordinates the naming of a the add-function-to-python-module function
fn function_wrapper_ident(name: &Ident) -> Ident {
    // Make sure this ident matches the one of wrap_pyfunction
    format_ident!("__pyo3_get_function_{}", name)
}

/// Generates python wrapper over a function that allows adding it to a python module as a python
/// function
pub fn add_fn_to_module(
    func: &mut syn::ItemFn,
    python_name: Ident,
    pyfn_attrs: Vec<pyfunction::Argument>,
) -> syn::Result<TokenStream> {
    let mut arguments = Vec::new();

    for input in func.sig.inputs.iter() {
        match input {
            syn::FnArg::Receiver(_) => {
                return Err(syn::Error::new_spanned(
                    input,
                    "Unexpected receiver for #[pyfn]",
                ))
            }
            syn::FnArg::Typed(ref cap) => {
                arguments.push(wrap_fn_argument(cap, &func.sig.ident)?);
            }
        }
    }

    let ty = method::get_return_info(&func.sig.output);

    let text_signature = utils::parse_text_signature_attrs(&mut func.attrs, &python_name)?;
    let doc = utils::get_doc(&func.attrs, text_signature, true)?;

    let function_wrapper_ident = function_wrapper_ident(&func.sig.ident);

    let spec = method::FnSpec {
        tp: method::FnType::FnStatic,
        name: &function_wrapper_ident,
        python_name,
        attrs: pyfn_attrs,
        args: arguments,
        output: ty,
        doc,
    };

    let doc = &spec.doc;

    let python_name = &spec.python_name;

    let wrapper = function_c_wrapper(&func.sig.ident, &spec);

    Ok(quote! {
        fn #function_wrapper_ident(py: pyo3::Python) -> pyo3::PyObject {
            #wrapper

            let _def = pyo3::class::PyMethodDef {
                ml_name: stringify!(#python_name),
                ml_meth: pyo3::class::PyMethodType::PyCFunctionWithKeywords(__wrap),
                ml_flags: pyo3::ffi::METH_VARARGS | pyo3::ffi::METH_KEYWORDS,
                ml_doc: #doc,
            };

            let function = unsafe {
                pyo3::PyObject::from_owned_ptr_or_panic(
                    py,
                    pyo3::ffi::PyCFunction_New(
                        Box::into_raw(Box::new(_def.as_method_def())),
                        ::std::ptr::null_mut()
                    )
                )
            };

            function
        }
    })
}

/// Generate static function wrapper (PyCFunction, PyCFunctionWithKeywords)
fn function_c_wrapper(name: &Ident, spec: &method::FnSpec<'_>) -> TokenStream {
    let names: Vec<Ident> = get_arg_names(&spec);
    let cb = quote! {
        #name(#(#names),*)
    };

    let body = pymethod::impl_arg_params(spec, cb);

    quote! {
        unsafe extern "C" fn __wrap(
            _slf: *mut pyo3::ffi::PyObject,
            _args: *mut pyo3::ffi::PyObject,
            _kwargs: *mut pyo3::ffi::PyObject) -> *mut pyo3::ffi::PyObject
        {
            const _LOCATION: &'static str = concat!(stringify!(#name), "()");
            pyo3::callback_body!(_py, {
                let _args = _py.from_borrowed_ptr::<pyo3::types::PyTuple>(_args);
                let _kwargs: Option<&pyo3::types::PyDict> = _py.from_borrowed_ptr_or_opt(_kwargs);

                #body
            })
        }
    }
}
