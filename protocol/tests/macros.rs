#[allow(unused_macros)]
macro_rules! serde_type {
    ($type:ty, $value:expr) => {{
        let mut writer = UperWriter::default();
        writer.write(&$value).unwrap();
        let mut reader = writer.as_reader();
        let deser = reader.read::<$type>().unwrap();
        assert_eq!($value, deser);
    }};
}
