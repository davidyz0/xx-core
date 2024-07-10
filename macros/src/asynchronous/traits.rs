use super::*;

fn format_fn_ident(ident: &Ident) -> Ident {
	format_ident!("__xx_async_impl_{}", ident)
}

fn format_trait_ident(ident: &Ident) -> Ident {
	format_ident!("{}Ext", ident)
}

fn wrapper_function(func: &mut TraitItemFn, this: &TokenStream) {
	let mut call = Vec::new();
	let ident = format_fn_ident(&func.sig.ident);

	call.push(quote! { #this::#ident });

	if !func.sig.generics.params.is_empty() {
		let mut generics = func.sig.generics.clone();

		generics.params = generics
			.params
			.into_iter()
			.filter(|generic| !matches!(generic, GenericParam::Lifetime(_)))
			.collect();
		let (_, type_generics, _) = generics.split_for_impl();

		call.push(type_generics.as_turbofish().into_token_stream());
	}

	let mut args = get_args(&func.sig.inputs, true);

	if func.sig.asyncness.is_some() {
		let context = Context::ident();

		args.push(parse_quote! { #context });
	}

	call.push(quote! { (#args) });

	func.default = Some(parse_quote! {{ unsafe { #(#call)* } }});
	func.attrs.push(parse_quote! {
		#[allow(unsafe_op_in_unsafe_fn, clippy::used_underscore_binding)]
	});

	RemoveModifiers {}.visit_signature_mut(&mut func.sig);
}

fn trait_ext(mut attrs: AttributeArgs, mut ext: ItemTrait) -> Result<TokenStream> {
	let name = ext.ident.clone();

	ext.ident = format_trait_ident(&name);

	let (_, type_generics, where_clause) = ext.generics.split_for_impl();
	let super_trait: TypeParamBound = parse_quote! { #name #type_generics };

	ext.supertraits.push(super_trait.clone());
	attrs.async_kind.0 = AsyncKind::TraitExt;

	let this = quote! { <Self as #super_trait> };

	for trait_item in take(&mut ext.items) {
		let TraitItem::Fn(mut func) = trait_item else {
			continue;
		};

		wrapper_function(&mut func, &this);

		if func.sig.asyncness.is_some() {
			transform_async(
				attrs.clone(),
				&mut Function::from_trait_fn(false, Some(&ext.generics), &mut func)
			)?;
		}

		ext.items.push(TraitItem::Fn(func));
	}

	let ident = &ext.ident;
	let mut new_generics = ext.generics.clone();

	new_generics.params.push(parse_quote! {
		XXInternalTraitImplementer: #super_trait + ?Sized
	});

	Ok(quote! {
		#[cfg(not(doc))]
		#ext

		#[cfg(not(doc))]
		impl #new_generics #ident #type_generics for XXInternalTraitImplementer #where_clause {}
	})
}

fn async_impl_fn(attrs: &AttributeArgs, func: &mut Function<'_>) -> Result<()> {
	func.is_root = false;
	func.sig.ident = format_fn_ident(&func.sig.ident);

	if func.sig.asyncness.is_some() && func.sig.unsafety.is_none() {
		/* caller must ensure we're allowed to suspend */
		func.sig.unsafety = Some(Default::default());
	}

	func.attrs.push(parse_quote! {
		#[doc(hidden)]
	});

	if func.sig.asyncness.is_some() {
		transform_async(attrs.clone(), func)?;
	}

	Ok(())
}

macro_rules! assign_default {
	($item:ident, $this:ident) => {
		let ident = &$item.ident;

		$item.default = Some((Default::default(), parse_quote! { #$this::#ident }));
	};
}

fn gen_impl<F>(imp: &ItemTrait, mut attrs: AttributeArgs, type_map: F) -> Result<TokenStream>
where
	F: FnOnce(TokenStream) -> TokenStream
{
	let name = &imp.ident;
	let (_, type_generics, where_clause) = imp.generics.split_for_impl();

	attrs.async_kind.0 = AsyncKind::TraitFn;

	let this = quote! { XXInternalTraitImplementer };
	let mut items = Vec::new();

	for trait_item in &imp.items {
		match trait_item.clone() {
			TraitItem::Const(mut item) => {
				assign_default!(item, this);

				items.push(TraitItem::Const(item));
			}

			TraitItem::Type(mut item) => {
				assign_default!(item, this);

				items.push(TraitItem::Type(item));
			}

			TraitItem::Fn(mut func) => {
				wrapper_function(&mut func, &this);

				async_impl_fn(
					&attrs,
					&mut Function::from_trait_fn(false, Some(&imp.generics), &mut func)
				)?;

				items.push(TraitItem::Fn(func));
			}

			_ => ()
		}
	}

	let mut new_generics = imp.generics.clone();

	new_generics.params.push(parse_quote! {
		XXInternalTraitImplementer: #name #type_generics + ?Sized
	});

	let ty = type_map(quote! { XXInternalTraitImplementer });

	Ok(quote! {
		impl #new_generics #name #type_generics for #ty #where_clause {
			#(#items)*
		}
	})
}

fn gen_impls(attrs: &AttributeArgs, item: &ItemTrait) -> Result<TokenStream> {
	let mut impls = Vec::<TokenStream>::new();
	let impl_gen = &attrs.impl_gen;

	if impl_gen.impl_ref.is_some() {
		impls.push(gen_impl(item, attrs.clone(), |ty| quote! { &#ty })?);
	}

	if impl_gen.impl_mut.is_some() {
		impls.push(gen_impl(item, attrs.clone(), |ty| quote! { &mut #ty })?);
	}

	if impl_gen.impl_box.is_some() {
		impls.push(gen_impl(
			item,
			attrs.clone(),
			|ty| quote! { ::std::boxed::Box<#ty> }
		)?);
	}

	Ok(quote! { #(#impls)* })
}

pub fn async_trait(mut attrs: AttributeArgs, item: ItemTrait) -> Result<TokenStream> {
	let ext = trait_ext(attrs.clone(), item.clone())?;
	let impls = gen_impls(&attrs, &item)?;

	attrs.async_kind.0 = AsyncKind::TraitFn;

	let functions = Functions::Trait(item);
	let item = functions.transform_all(|func| async_impl_fn(&attrs, func), |_| true)?;

	Ok(quote! { #item #ext #impls })
}

pub fn async_impl(mut attrs: AttributeArgs, item: Functions) -> Result<TokenStream> {
	attrs.async_kind.0 = AsyncKind::TraitFn;

	item.transform_all(
		|func| async_impl_fn(&attrs, func),
		|item| matches!(item, Functions::Fn(_) | Functions::Impl(_))
	)
}
