pub use bytes;
pub use paste::paste;

/// Macro to derive [`EpeeObject`](crate::EpeeObject) for structs.
///
/// ### Basic Usage:
///
/// ```rust
/// // mod visibility is here because of Rust visibility weirdness, you shouldn't need this unless defined in a function.
/// // see: <https://github.com/rust-lang/rust/issues/64079>
/// mod visibility {
///
///     use xkr_epee::epee_object;
///
///     struct Example {
///         a: u8
///     }
///
///     epee_object!(
///         Example,
///         a: u8,
///     );
/// }
/// ```
///
/// ### Advanced Usage:
///
/// ```rust
/// // mod visibility is here because of Rust visibility weirdness, you shouldn't need this unless defined in a function.
/// // see: <https://github.com/rust-lang/rust/issues/64079>
/// mod visibility {
///
///     use xkr_epee::epee_object;
///
///     struct Example {
///         a: u8,
///         b: u8,
///         c: u8,
///         d: u8,
///         e: Vec<String>,
///         f_f: Example2
///     }
///
///     struct Example2 {
///         f: u8
///     }
///
///     epee_object!(
///         Example2,
///         f: u8,
///     );
///
///     epee_object!(
///         Example,
///         // `("ALT-NAME")` changes the name of the field in the encoded data.
///         a("A"): u8,
///         // `= VALUE` sets a default value that this field will be set to if not in the data
///         // when encoding this field will be skipped if equal to the default.
///         b: u8 = 0,
///         // `as ALT-TYPE` encodes the data using the alt type, the alt type must impl Into<Type> and From<&Type>
///         c: u8 as u8,
///         // `=> read_fn, write_fn, should_write_fn,` allows you to specify alt field encoding functions.
///         //  for the required args see the default functions, which are used here:
///         d: u8 => xkr_epee::read_epee_value, xkr_epee::write_field, <u8 as xkr_epee::EpeeValue>::should_write,
///         // `[]` allows you to set limits on fields.
///         // `[min_element: NUMBER]` is used to set a minimum number of bytes that each element must consume.
///         // `[max_len: NUMBER]` is used to set a maximum number of elements in a sequence.
///         e: Vec<String> [min_element: 16, max_len: 128],
///         // `!flatten` can be used on fields which are epee objects, and it flattens the fields of that object into this object.
///         // So for this example `f_f` will not appear in the data but f will.
///         // You can't use the other options with this.
///         !flatten: f_f: Example2,
///     );
/// }
/// ```
///
///
#[macro_export]
macro_rules! epee_object {
    // ------------------------------------------------------------------------ internal_try_right_then_left
    // All this does is return the second (right) arg if present otherwise the left is returned.
    (
        @internal_try_right_then_left
        $a:expr_2021, $b:expr_2021
    ) => {
        $b
    };

    (
        @internal_try_right_then_left
        $a:expr_2021,
    ) => {
        $a
    };

    // ------------------------------------------------------------------------ internal_field_name
    // Returns the alt_name if present otherwise stringifies the field ident.
    (
        @internal_field_name
        $field: tt, $alt_name: tt
    ) => {
        $alt_name
    };

    (
        @internal_field_name
        $field: ident,
    ) => {
        stringify!($field)
    };

    // ------------------------------------------------------------------------ internal_field_type
    // All this does is return the second (right) arg if present otherwise the left is returned.
    (
        @internal_field_type
        $ty:ty, $ty_as:ty
    ) => {
        $ty_as
    };
    (
        @internal_field_type
        $ty:ty,
    ) => {
        $ty
    };

    // ------------------------------------------------------------------------ internal_sequence_limits
    (
        @internal_sequence_limits
    ) => {
        xkr_epee::EpeeValueLimits {
            min_element_size: 0,
            max_sequence_len: usize::MAX,
        }
    };

    (
        @internal_sequence_limits
        min_element: $min_element_size:expr_2021
    ) => {
        xkr_epee::EpeeValueLimits {
            min_element_size: $min_element_size,
            max_sequence_len: usize::MAX,
        }
    };

    (
        @internal_sequence_limits
        max_len: $max_sequence_len:expr_2021
    ) => {
        xkr_epee::EpeeValueLimits {
            min_element_size: 0,
            max_sequence_len: $max_sequence_len,
        }
    };

    (
        @internal_sequence_limits
        min_element: $min_element_size:expr_2021, max_len: $max_sequence_len:expr_2021
    ) => {
        xkr_epee::EpeeValueLimits {
            min_element_size: $min_element_size,
            max_sequence_len: $max_sequence_len,
        }
    };

    // ------------------------------------------------------------------------ Entry Point
    (
        $obj:ident,
        $($field: ident $(($alt_name: literal))?: $ty:ty $(as $ty_as:ty )? $([$($sequence_constraints:tt)+])? $(= $default:expr_2021)?  $(=> $read_fn:expr_2021, $write_fn:expr_2021, $should_write_fn:expr_2021)?, )*
        $(!flatten: $flat_field: ident: $flat_ty:ty ,)*

    ) => {
        xkr_epee::macros::paste!(
            #[allow(non_snake_case, clippy::empty_structs_with_brackets)]
            mod [<__epee_builder_ $obj>] {
                use super::*;

                #[derive(Default)]
                #[allow(clippy::empty_structs_with_brackets)]
                pub struct [<__Builder $obj>] {
                    $($field: Option<xkr_epee::epee_object!(@internal_field_type $ty, $($ty_as)?)>,)*
                    $($flat_field: <$flat_ty as xkr_epee::EpeeObject>::Builder,)*
                }

                impl xkr_epee::EpeeObjectBuilder<$obj> for [<__Builder $obj>] {
                    fn add_field<B: xkr_epee::macros::bytes::Buf>(&mut self, name: &str, b: &mut B) -> xkr_epee::error::Result<bool> {
                        match name {
                            $(xkr_epee::epee_object!(@internal_field_name $field, $($alt_name)?) => {
                                let limits = xkr_epee::epee_object!(
                                    @internal_sequence_limits $($($sequence_constraints)+)?
                                );
                                if self.$field.replace(
                                    xkr_epee::epee_object!(@internal_try_right_then_left xkr_epee::read_epee_value(b, limits)?, $($read_fn(b, limits)?)?)
                                ).is_some() {
                                    Err(xkr_epee::error::Error::Value(format!("Duplicate field in data: {}", xkr_epee::epee_object!(@internal_field_name$field, $($alt_name)?))))?;
                                }
                                Ok(true)
                            },)*
                            _ => {

                                $(if self.$flat_field.add_field(name, b)? {
                                    return Ok(true);
                                })*

                                Ok(false)
                            }
                        }
                    }

                    fn finish(self) -> xkr_epee::error::Result<$obj> {
                        Ok(
                            $obj {
                                $(
                                  $field: {
                                      let epee_default_value = xkr_epee::epee_object!(@internal_try_right_then_left xkr_epee::EpeeValue::epee_default_value(), $({
                                            let _ = $should_write_fn;
                                            None
                                      })?);

                                      self.$field
                                            $(.or(Some($default)))?
                                            .or(epee_default_value)
                                            $(.map(<$ty_as>::into))?
                                              .ok_or(xkr_epee::error::Error::Value(format!("Missing field in data: {}", xkr_epee::epee_object!(@internal_field_name$field, $($alt_name)?))))?
                                  },
                                )*

                                $(
                                    $flat_field: self.$flat_field.finish()?,
                                )*

                            }
                        )
                    }
                }
            }

            impl xkr_epee::EpeeObject for $obj {
                type Builder = [<__epee_builder_ $obj>]::[<__Builder $obj>];

                fn number_of_fields(&self) -> u64 {
                    let mut fields = 0;

                    $(
                    let field = xkr_epee::epee_object!(@internal_try_right_then_left &self.$field, $(<&$ty_as>::from(&self.$field))? );

                      if $((field) != &$default &&)? xkr_epee::epee_object!(@internal_try_right_then_left xkr_epee::EpeeValue::should_write, $($should_write_fn)?)(field )
                      {
                          fields += 1;
                      }
                    )*

                    $(
                        fields += self.$flat_field.number_of_fields();
                    )*

                    fields
                }

                fn write_fields<B: xkr_epee::macros::bytes::BufMut>(self, w: &mut B) -> xkr_epee::error::Result<()> {
                    $(
                    let field = xkr_epee::epee_object!(@internal_try_right_then_left self.$field, $(<$ty_as>::from(self.$field))? );

                      if $(field != $default &&)? xkr_epee::epee_object!(@internal_try_right_then_left xkr_epee::EpeeValue::should_write, $($should_write_fn)?)(&field )
                        {
                         xkr_epee::epee_object!(@internal_try_right_then_left xkr_epee::write_field, $($write_fn)?)((field), xkr_epee::epee_object!(@internal_field_name$field, $($alt_name)?), w)?;
                      }
                    )*

                    $(
                        self.$flat_field.write_fields(w)?;
                    )*

                    Ok(())
                }
            }
        );
    };
}
