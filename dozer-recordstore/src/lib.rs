//! [`RecordRef`] is a compact representation of a collection of [dozer_types::types::Field]s
//! There are two principles that make this representation more compact than `[Field]`:
//!  1. The fields and their types are stored as a Struct of Arrays instead of
//!     and Array of Structs. This makes it possible to pack the discriminants
//!     for the field types as a byte per field, instead of taking up a full word,
//!     which is the case in [Field] (because the variant value must be aligned)
//!  2. The field values are stored packed. In a `[Field]` representation, each
//!     field takes as much space as the largest enum variant in [Field] (plus its discriminant,
//!     see (1.)). Instead, for the compact representation, we pack the values into
//!     align_of::<Field>() sized slots. This way, a u64 takes only 8 bytes, whereas
//!     a u128 can still use its 16 bytes.
use std::alloc::{dealloc, handle_alloc_error, Layout};
use std::{hash::Hash, ptr::NonNull};

use triomphe::{Arc, HeaderSlice};

use dozer_types::chrono::{DateTime, FixedOffset, NaiveDate};
use dozer_types::json_types::JsonValue;
use dozer_types::ordered_float::OrderedFloat;
use dozer_types::rust_decimal::Decimal;
use dozer_types::types::{DozerDuration, DozerPoint};
use dozer_types::{
    serde::{Deserialize, Serialize},
    types::{Field, FieldType, Lifetime},
};

// The alignment of an enum is necessarily the maximum alignment of its variants
// (otherwise it would be unsound to read from it).
// So, by using the alignment of `Field` as the alignment of the values in our
// packed `RecordRef`, we ensure that all accesses are aligned.
// This wastes a little bit of memory for subsequent fields that have
// smaller minimum alignment and size (such as `bool`, which has size=1, align=1),
// but in practice this should be negligible compared to the added effort of
// packing these fields while keeping everything aligned.
const MAX_ALIGN: usize = std::mem::align_of::<Field>();

#[repr(transparent)]
#[derive(Debug)]
/// `repr(transparent)` inner struct so we can implement drop logic on it
/// This is a `triomphe` HeaderSlice so we can make a fat Arc, saving a level
/// of indirection and a pointer which would otherwise be needed for the field types
struct RecordRefInner(HeaderSlice<NonNull<u8>, [Option<FieldType>]>);

#[derive(Debug, Clone)]
pub struct RecordRef(Arc<RecordRefInner>);

impl PartialEq for RecordRef {
    fn eq(&self, other: &Self) -> bool {
        self.load() == other.load()
    }
}

impl Eq for RecordRef {}

unsafe impl Send for RecordRef {}
unsafe impl Sync for RecordRef {}

impl Hash for RecordRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.load().hash(state)
    }
}

impl<'de> Deserialize<'de> for RecordRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: dozer_types::serde::Deserializer<'de>,
    {
        let fields = Vec::<FieldRef>::deserialize(deserializer)?;
        let owned_fields: Vec<_> = fields.iter().map(FieldRef::cloned).collect();
        Ok(Self::new(owned_fields))
    }
}
impl Serialize for RecordRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: dozer_types::serde::Serializer,
    {
        self.load().serialize(serializer)
    }
}

#[inline(always)]
unsafe fn adjust_alignment<T>(ptr: *mut u8) -> *mut u8 {
    ptr.add(ptr.align_offset(std::mem::align_of::<T>()))
}
/// # Safety
/// ptr should be valid for writing a `T`,
/// that is, ptr..ptr + size_of::<T> should be inside a single live allocation
unsafe fn write<T>(ptr: *mut u8, value: T) -> *mut u8 {
    let ptr = adjust_alignment::<T>(ptr) as *mut T;
    ptr.write(value);
    ptr.add(1) as *mut u8
}

/// # Safety
/// ptr should be valid for reading a `T`,
/// that is, ptr..ptr + size_of::<T> should be inside a single live allocation
/// and the memory read should be initialized.
/// The returned reference is only valid as long as pointed to memory is valid
/// for reading.
unsafe fn read_ref<'a, T>(ptr: *mut u8) -> (*mut u8, &'a T) {
    let ptr = adjust_alignment::<T>(ptr) as *mut T;
    let result = &*ptr;
    (ptr.add(1) as *mut u8, result)
}

/// # Safety
/// ptr should be valid for reading a `T`,
/// that is, ptr..ptr + size_of::<T> should be inside a single live allocation
/// and the memory read should be initialized.
/// This takes ownership of the memory returned as `T`, which means dropping `T`
/// may make future reads from `ptr` undefined behavior
unsafe fn read<T>(ptr: *mut u8) -> (*mut u8, T) {
    let ptr = adjust_alignment::<T>(ptr) as *mut T;
    let result = ptr.read();
    (ptr.add(1) as *mut u8, result)
}

/// # Safety
/// `ptr` should be valid for reading the contents of a `Field` with the type
/// corresponding to `field_type`.
/// See `read_ref`
unsafe fn read_field_ref<'a>(ptr: *mut u8, field_type: FieldType) -> (*mut u8, FieldRef<'a>) {
    match field_type {
        FieldType::UInt => {
            let (ptr, value) = read_ref(ptr);
            (ptr, FieldRef::UInt(*value))
        }
        FieldType::U128 => {
            let (ptr, value) = read_ref(ptr);
            (ptr, FieldRef::U128(*value))
        }

        FieldType::Int => {
            let (ptr, value) = read_ref(ptr);
            (ptr, FieldRef::Int(*value))
        }

        FieldType::I128 => {
            let (ptr, value) = read_ref(ptr);
            (ptr, FieldRef::I128(*value))
        }

        FieldType::Float => {
            let (ptr, value) = read_ref(ptr);
            (ptr, FieldRef::Float(*value))
        }

        FieldType::Boolean => {
            let (ptr, value) = read_ref(ptr);
            (ptr, FieldRef::Boolean(*value))
        }

        FieldType::String => {
            let (ptr, value): (_, &String) = read_ref(ptr);
            (ptr, FieldRef::String(value))
        }
        FieldType::Text => {
            let (ptr, value): (_, &String) = read_ref(ptr);
            (ptr, FieldRef::Text(value))
        }
        FieldType::Binary => {
            let (ptr, value): (_, &Vec<u8>) = read_ref(ptr);
            (ptr, FieldRef::Binary(value))
        }
        FieldType::Decimal => {
            let (ptr, value) = read_ref(ptr);
            (ptr, FieldRef::Decimal(*value))
        }
        FieldType::Timestamp => {
            let (ptr, value) = read_ref(ptr);
            (ptr, FieldRef::Timestamp(*value))
        }
        FieldType::Date => {
            let (ptr, value) = read_ref(ptr);
            (ptr, FieldRef::Date(*value))
        }
        FieldType::Json => {
            let (ptr, value) = read_ref::<JsonValue>(ptr);
            (ptr, FieldRef::Json(value.to_owned()))
        }
        FieldType::Point => {
            let (ptr, value) = read_ref(ptr);
            (ptr, FieldRef::Point(*value))
        }
        FieldType::Duration => {
            let (ptr, value) = read_ref(ptr);
            (ptr, FieldRef::Duration(*value))
        }
    }
}
unsafe fn read_field(ptr: *mut u8, field_type: FieldType) -> (*mut u8, Field) {
    match field_type {
        FieldType::UInt => {
            let (ptr, value) = read(ptr);
            (ptr, Field::UInt(value))
        }
        FieldType::U128 => {
            let (ptr, value) = read(ptr);
            (ptr, Field::U128(value))
        }

        FieldType::Int => {
            let (ptr, value) = read(ptr);
            (ptr, Field::Int(value))
        }

        FieldType::I128 => {
            let (ptr, value) = read(ptr);
            (ptr, Field::I128(value))
        }

        FieldType::Float => {
            let (ptr, value) = read(ptr);
            (ptr, Field::Float(value))
        }

        FieldType::Boolean => {
            let (ptr, value) = read(ptr);
            (ptr, Field::Boolean(value))
        }

        FieldType::String => {
            let (ptr, value) = read(ptr);
            (ptr, Field::String(value))
        }
        FieldType::Text => {
            let (ptr, value) = read(ptr);
            (ptr, Field::String(value))
        }
        FieldType::Binary => {
            let (ptr, value) = read(ptr);
            (ptr, Field::Binary(value))
        }
        FieldType::Decimal => {
            let (ptr, value) = read(ptr);
            (ptr, Field::Decimal(value))
        }
        FieldType::Timestamp => {
            let (ptr, value) = read(ptr);
            (ptr, Field::Timestamp(value))
        }
        FieldType::Date => {
            let (ptr, value) = read(ptr);
            (ptr, Field::Date(value))
        }
        FieldType::Json => {
            let (ptr, value) = read::<JsonValue>(ptr);
            (ptr, Field::Json(value))
        }
        FieldType::Point => {
            let (ptr, value) = read(ptr);
            (ptr, Field::Point(value))
        }
        FieldType::Duration => {
            let (ptr, value) = read(ptr);
            (ptr, Field::Duration(value))
        }
    }
}

#[inline(always)]
fn add_field_size<T>(size: &mut usize) {
    let align = std::mem::align_of::<T>();
    // Align the start of the field
    *size = (*size + (align - 1)) & !(align - 1);
    *size += std::mem::size_of::<T>();
}
fn size(fields: &[Option<FieldType>]) -> usize {
    let mut size = 0;
    for field in fields.iter().flatten() {
        match field {
            FieldType::UInt => add_field_size::<u64>(&mut size),
            FieldType::U128 => add_field_size::<u128>(&mut size),
            FieldType::Int => add_field_size::<i64>(&mut size),
            FieldType::I128 => add_field_size::<i128>(&mut size),
            FieldType::Float => add_field_size::<OrderedFloat<f64>>(&mut size),
            FieldType::Boolean => add_field_size::<bool>(&mut size),
            FieldType::String => add_field_size::<String>(&mut size),
            FieldType::Text => add_field_size::<String>(&mut size),
            FieldType::Binary => add_field_size::<Vec<u8>>(&mut size),
            FieldType::Decimal => add_field_size::<Decimal>(&mut size),
            FieldType::Timestamp => add_field_size::<DateTime<FixedOffset>>(&mut size),
            FieldType::Date => add_field_size::<NaiveDate>(&mut size),
            FieldType::Json => add_field_size::<JsonValue>(&mut size),
            FieldType::Point => add_field_size::<DozerPoint>(&mut size),
            FieldType::Duration => add_field_size::<DozerDuration>(&mut size),
        }
    }
    size
}

#[derive(Hash, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(crate = "dozer_types::serde")]
pub enum FieldRef<'a> {
    UInt(u64),
    U128(u128),
    Int(i64),
    I128(i128),
    Float(OrderedFloat<f64>),
    Boolean(bool),
    String(&'a str),
    Text(&'a str),
    Binary(&'a [u8]),
    Decimal(Decimal),
    Timestamp(DateTime<FixedOffset>),
    Date(NaiveDate),
    Json(JsonValue),
    Point(DozerPoint),
    Duration(DozerDuration),
    Null,
}

impl FieldRef<'_> {
    pub fn cloned(&self) -> Field {
        match self {
            FieldRef::UInt(v) => Field::UInt(*v),
            FieldRef::U128(v) => Field::U128(*v),
            FieldRef::Int(v) => Field::Int(*v),
            FieldRef::I128(v) => Field::I128(*v),
            FieldRef::Float(v) => Field::Float(*v),
            FieldRef::Boolean(v) => Field::Boolean(*v),
            FieldRef::String(v) => Field::String((*v).to_owned()),
            FieldRef::Text(v) => Field::Text((*v).to_owned()),
            FieldRef::Binary(v) => Field::Binary((*v).to_vec()),
            FieldRef::Decimal(v) => Field::Decimal(*v),
            FieldRef::Timestamp(v) => Field::Timestamp(*v),
            FieldRef::Date(v) => Field::Date(*v),
            FieldRef::Json(v) => Field::Json(v.clone()),
            FieldRef::Point(v) => Field::Point(*v),
            FieldRef::Duration(v) => Field::Duration(*v),
            FieldRef::Null => Field::Null,
        }
    }
}

impl RecordRef {
    pub fn new(fields: Vec<Field>) -> Self {
        let field_types = fields
            .iter()
            .map(|field| field.ty())
            .collect::<Box<[Option<FieldType>]>>();
        let size = size(&field_types);

        let layout = Layout::from_size_align(size, MAX_ALIGN).unwrap();
        // SAFETY: Everything is `ALIGN` byte aligned
        let data = unsafe {
            let data = std::alloc::alloc(layout);
            if data.is_null() {
                handle_alloc_error(layout);
            }
            data
        };
        // SAFETY: We checked for null above
        let data = unsafe { NonNull::new_unchecked(data) };
        let mut ptr = data.as_ptr();

        // SAFETY:
        // - ptr is non-null (we got it from a NonNull)
        // - ptr is dereferencable (its memory range is large enough and not de-allocated)
        //
        unsafe {
            for field in fields {
                match field {
                    Field::UInt(v) => ptr = write(ptr, v),
                    Field::U128(v) => ptr = write(ptr, v),
                    Field::Int(v) => ptr = write(ptr, v),
                    Field::I128(v) => ptr = write(ptr, v),
                    Field::Float(v) => ptr = write(ptr, v),
                    Field::Boolean(v) => ptr = write(ptr, v),
                    Field::String(v) => ptr = write(ptr, v),
                    Field::Text(v) => ptr = write(ptr, v),
                    Field::Binary(v) => ptr = write(ptr, v),
                    Field::Decimal(v) => ptr = write(ptr, v),
                    Field::Timestamp(v) => ptr = write(ptr, v),
                    Field::Date(v) => ptr = write(ptr, v),
                    Field::Json(v) => ptr = write(ptr, v),
                    Field::Point(v) => ptr = write(ptr, v),
                    Field::Duration(v) => ptr = write(ptr, v),
                    Field::Null => (),
                }
            }
        }
        // SAFETY: This is valid, because inner is `repr(transparent)`
        let arc = unsafe {
            let arc = Arc::from_header_and_slice(data, &field_types);
            std::mem::transmute(arc)
        };
        Self(arc)
    }

    pub fn load(&self) -> Vec<FieldRef<'_>> {
        self.0
            .field_types()
            .iter()
            .scan(self.0.data().as_ptr(), |ptr, field_type| {
                let Some(field_type) = field_type else {
                    return Some(FieldRef::Null);
                };

                unsafe {
                    let (new_ptr, value) = read_field_ref(*ptr, *field_type);
                    *ptr = new_ptr;
                    Some(value)
                }
            })
            .collect()
    }

    #[inline(always)]
    pub fn id(&self) -> usize {
        self.0.as_ptr() as *const () as usize
    }
}

impl RecordRefInner {
    #[inline(always)]
    fn field_types(&self) -> &[Option<FieldType>] {
        &self.0.slice
    }

    #[inline(always)]
    fn data(&self) -> NonNull<u8> {
        self.0.header
    }
}

impl Drop for RecordRefInner {
    fn drop(&mut self) {
        let mut ptr = self.data().as_ptr();
        for field in self.field_types().iter().flatten() {
            unsafe {
                // Read owned so all field destructors run
                ptr = read_field(ptr, *field).0;
            }
        }
        // Then deallocate the field storage
        unsafe {
            dealloc(
                self.data().as_ptr(),
                Layout::from_size_align(size(self.field_types()), MAX_ALIGN).unwrap(),
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct ProcessorRecord {
    /// All `Field`s in this record. The `Field`s are grouped by `Arc` to reduce memory usage.
    /// This is a Box<[]> instead of a Vec to save space on storing the vec's capacity
    values: Box<[RecordRef]>,

    /// Time To Live for this record. If the value is None, the record will never expire.
    lifetime: Option<Box<Lifetime>>,
}

impl ProcessorRecord {
    pub fn new(values: Box<[RecordRef]>) -> Self {
        Self {
            values,
            ..Default::default()
        }
    }

    pub fn get_lifetime(&self) -> Option<Lifetime> {
        self.lifetime.as_ref().map(|lifetime| *lifetime.clone())
    }
    pub fn set_lifetime(&mut self, lifetime: Option<Lifetime>) {
        self.lifetime = lifetime.map(Box::new);
    }

    pub fn values(&self) -> &[RecordRef] {
        &self.values
    }

    pub fn appended(existing: &ProcessorRecord, additional: RecordRef) -> Self {
        let mut values = Vec::with_capacity(existing.values().len() + 1);
        values.extend_from_slice(existing.values());
        values.push(additional);
        Self::new(values.into_boxed_slice())
    }
}

mod store;
pub use store::{ProcessorRecordStore, RecordStoreError};

#[cfg(test)]
mod tests {
    use dozer_types::types::Field;

    use crate::RecordRef;

    #[test]
    fn test_store_load() {
        let fields = vec![
            Field::String("asdf".to_owned()),
            Field::Int(23),
            Field::Null,
            Field::U128(234),
        ];

        let record = RecordRef::new(fields.clone());
        let loaded_fields: Vec<_> = record
            .load()
            .into_iter()
            .map(|field| field.cloned())
            .collect();
        assert_eq!(&fields, &loaded_fields);
    }

    #[test]
    fn test_ser_de() {
        let fields = vec![
            Field::String("asdf".to_owned()),
            Field::Int(23),
            Field::Null,
            Field::U128(234),
        ];

        let record = RecordRef::new(fields.clone());

        let bytes = dozer_types::bincode::serialize(&record).unwrap();
        let deserialized: RecordRef = dozer_types::bincode::deserialize(&bytes).unwrap();
        let loaded_fields: Vec<_> = deserialized
            .load()
            .into_iter()
            .map(|field| field.cloned())
            .collect();
        assert_eq!(&fields, &loaded_fields);
    }
}