pub mod common_types;
pub mod control_messages;
pub mod event_messages;
pub mod media_messages;

#[cfg(test)]
pub(crate) mod test {

    macro_rules! serde_type_der {
        ($type:ty, $value:expr) => {{
            let encoded = rasn::der::encode(&$value).expect("encode");
            let decoded: $type = rasn::der::decode(&encoded).expect("decode");
            pretty_assertions::assert_eq!(decoded, $value);
        }};
    }
    pub(crate) use serde_type_der;

    #[allow(
        non_snake_case,
        non_upper_case_globals,
        non_camel_case_types,
        unused,
        improper_ctypes
    )]
    pub mod c {
        use std::ffi::c_void;
        use std::ops::{Deref, DerefMut};

        macro_rules! round_trip_der {
            ($rust_type:ty, $c_type:ty, $value:expr) => {{
                let rasn_encoded = rasn::ber::encode(&$value).expect("rasn encode");
                let asn1c_decoded: crate::test::c::ASNBox<$c_type> =
                    crate::test::c::der_decode_full(&rasn_encoded).expect("asn1c decode");
                let mut buf = vec![0_u8; rasn_encoded.len()];
                let asn1c_encoded = crate::test::c::der_encode_full(&*asn1c_decoded, &mut buf)
                    .expect("asn1c encode");
                pretty_assertions::assert_eq!(rasn_encoded, asn1c_encoded);
                let rasn_decoded: $rust_type =
                    rasn::der::decode(&asn1c_encoded).expect("rasn decode");
                pretty_assertions::assert_eq!($value, rasn_decoded);
            }};
        }
        pub(crate) use round_trip_der;

        include!(concat!(env!("OUT_DIR"), "/c/bindings/bindings.rs"));

        // Much of the following is "borrowed" from
        // https://sjames.github.io/articles/2020-04-26-rust-ffi-asn1-codec/
        // This is only used in tests, so the unsafe stuff is less concerning than if it were in
        // production
        pub trait ASN1GenType {
            unsafe fn get_descriptor() -> &'static asn_TYPE_descriptor_t;
        }

        enum AllocatedData<T: Sized + ASN1GenType> {
            RustAllocated(*mut T),
            Asn1CodecAllocated(*mut T),
        }

        impl<T> std::fmt::Debug for AllocatedData<T>
        where
            T: std::fmt::Debug + Sized + ASN1GenType,
        {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
                let inner = match self {
                    AllocatedData::RustAllocated(inner) => inner,
                    AllocatedData::Asn1CodecAllocated(inner) => inner,
                };
                let t: &T = unsafe { &**inner };
                write!(f, "inner: {:?}", t)
            }
        }

        #[derive(Debug)]
        pub struct ASNBox<T>(AllocatedData<T>)
        where
            T: Sized + ASN1GenType;

        impl<T> ASNBox<T>
        where
            T: Sized + ASN1GenType,
        {
            pub unsafe fn new_from_asn1codec_allocated_struct(p: *mut T) -> Self {
                if !p.is_null() {
                    Self(AllocatedData::<T>::Asn1CodecAllocated(p))
                } else {
                    panic!("Tried to create ASNBox from null pointer");
                }
            }

            pub fn new_from_box(b: Box<T>) -> Self {
                Self(AllocatedData::RustAllocated(Box::into_raw(b)))
            }

            pub unsafe fn get_raw_mut_ptr(&self) -> *mut std::ffi::c_void {
                match self.0 {
                    AllocatedData::Asn1CodecAllocated(p) => p as *mut std::ffi::c_void,
                    AllocatedData::RustAllocated(p) => p as *mut std::ffi::c_void,
                }
            }
        }

        impl<T> Drop for ASNBox<T>
        where
            T: Sized + ASN1GenType,
        {
            fn drop(&mut self) {
                match self.0 {
                    AllocatedData::Asn1CodecAllocated(p) => unsafe {
                        let descriptor = T::get_descriptor();
                        let ops = descriptor.op.as_ref().unwrap();
                        let free_fn = ops.free_struct.unwrap();
                        free_fn(
                            descriptor,
                            p as *mut ::std::os::raw::c_void,
                            asn_struct_free_method_ASFM_FREE_EVERYTHING,
                        );
                    },
                    AllocatedData::RustAllocated(_) => {}
                }
            }
        }

        impl<T> Deref for ASNBox<T>
        where
            T: Sized + ASN1GenType,
        {
            type Target = T;
            fn deref(&self) -> &T {
                match self.0 {
                    AllocatedData::Asn1CodecAllocated(p) => unsafe { &*p as &T },
                    AllocatedData::RustAllocated(p) => unsafe { &*p as &T },
                }
            }
        }

        impl<T> DerefMut for ASNBox<T>
        where
            T: Sized + ASN1GenType,
        {
            fn deref_mut(&mut self) -> &mut T {
                match self.0 {
                    AllocatedData::Asn1CodecAllocated(p) => unsafe { &mut *p },
                    AllocatedData::RustAllocated(p) => unsafe { &mut *p },
                }
            }
        }

        pub fn der_encode_full<'a, T>(msg: &T, buffer: &'a mut [u8]) -> anyhow::Result<&'a [u8]>
        where
            T: Sized + ASN1GenType,
        {
            let message_ptr: *const c_void = msg as *const _ as *const c_void;
            let encode_buffer_ptr: *mut c_void = buffer.as_mut_ptr() as *mut _ as *mut c_void;

            unsafe {
                let enc_rval = der_encode_to_buffer(
                    T::get_descriptor(),
                    message_ptr,
                    encode_buffer_ptr,
                    buffer.len(),
                );
                if enc_rval.encoded < 0 {
                    anyhow::bail!("encoding failed");
                } else {
                    Ok(&buffer[0..enc_rval.encoded as usize])
                }
            }
        }

        pub fn der_decode_full<T>(buffer: &[u8]) -> anyhow::Result<ASNBox<T>>
        where
            T: Sized + ASN1GenType,
        {
            let codec_ctx = asn_codec_ctx_t { max_stack_size: 0 };
            let mut voidp: *mut c_void = std::ptr::null::<()>() as *mut c_void;
            let voidpp: *mut *mut c_void = &mut voidp;

            unsafe {
                let rval = ber_decode(
                    &codec_ctx as *const _,
                    T::get_descriptor(),
                    voidpp,
                    buffer.as_ptr() as *const ::std::os::raw::c_void,
                    buffer.len(),
                );
                if rval.code != asn_dec_rval_code_e_RC_OK {
                    anyhow::bail!("decoding failed");
                } else {
                    let msg = ASNBox::<T>::new_from_asn1codec_allocated_struct(voidp as *mut T);
                    Ok(msg)
                }
            }
        }
    }
}
