//! Lenient parsing of the Xtream player API's JSON lists.
//!
//! Panels are notoriously inconsistent: ids arrive as numbers, numeric
//! strings, or floats (`"12.0"`); optional fields as `null`, `""`, or not
//! at all; lists as arrays, as objects keyed by id, or as `{}` when
//! empty. Every record is therefore parsed on its own into an all-optional
//! raw form that accepts any JSON value per field, and only then turned
//! into a [`Category`] or [`LiveStream`]. A record that cannot be used
//! (say, a stream without a usable id) is skipped and counted, so one bad
//! entry among tens of thousands costs that entry, not the whole list.

use std::fmt;
use std::marker::PhantomData;

use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

use super::{Category, LiveStream, XtreamError};

/// Records parsed from one player-API list.
#[derive(Debug)]
pub(super) struct ApiList<T> {
    /// The usable records, in document order.
    pub(super) records: Vec<T>,
    /// Entries dropped because they could not be used (not an object, or
    /// missing a required field such as the stream id).
    pub(super) skipped: usize,
}

/// Parses a player-API reply holding a list of `R` records.
///
/// Accepts a JSON array of records, or an object whose values are the
/// records (keyed by id, which then also serves as a fallback id).
///
/// # Errors
///
/// [`XtreamError::Json`] when the reply is not well-formed JSON,
/// [`XtreamError::AuthFailed`] for the panel's "not authorized" account
/// object, and [`XtreamError::UnexpectedApiReply`] for any other reply
/// that is not a list.
pub(super) fn parse_api_list<R: RawRecord>(
    reader: impl std::io::Read,
) -> Result<ApiList<R::Record>, XtreamError> {
    // Straight from the byte stream into the records: no intermediate
    // `serde_json::Value` tree, which for a 20 MB stream list would cost
    // several times the body in short-lived allocations on top of the
    // records being built. Only one record's raw fields (and, for the
    // account object, the small `user_info`) exist as loose values.
    let mut deserializer = serde_json::Deserializer::from_reader(std::io::BufReader::new(reader));
    let outcome = (&mut deserializer).deserialize_any(ListVisitor::<R>(PhantomData))?;
    deserializer.end()?;
    match outcome {
        Outcome::List(list) => Ok(list),
        Outcome::AuthFailed => Err(XtreamError::AuthFailed),
        Outcome::Unexpected => Err(XtreamError::UnexpectedApiReply),
    }
}

/// Whether a `user_info` object reports that the credentials were
/// rejected (`auth` of `false`, `0`, or `"0"`).
pub(super) fn auth_failed(user_info: &serde_json::Value) -> bool {
    let Some(auth) = user_info.get("auth") else {
        return false;
    };
    matches!(auth, serde_json::Value::Bool(false))
        || auth.as_u64() == Some(0)
        || auth.as_str() == Some("0")
}

/// A JSON field value reduced to what the record fields need. Deserializes
/// from *any* JSON value and never fails on well-formed input; nested
/// arrays and objects are skipped without being built.
#[derive(Debug, Default, Clone, PartialEq)]
pub(super) enum Scalar {
    /// `null`, a boolean, an array, an object — or the field was missing.
    #[default]
    Absent,
    /// A JSON string, as sent.
    Text(String),
    /// A non-negative JSON integer.
    Unsigned(u64),
    /// A negative JSON integer.
    Signed(i64),
    /// A JSON number with a fraction or exponent.
    Float(f64),
}

impl Scalar {
    /// The value as non-empty, trimmed text: numbers in canonical form
    /// (integral floats without `.0`, so `12.0` and `12` agree). `None`
    /// for absent values and blank strings.
    fn into_text(self) -> Option<String> {
        match self {
            Self::Absent => None,
            Self::Text(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    None
                } else if trimmed.len() == text.len() {
                    Some(text)
                } else {
                    Some(trimmed.to_owned())
                }
            }
            Self::Unsigned(number) => Some(number.to_string()),
            Self::Signed(number) => Some(number.to_string()),
            Self::Float(number) => {
                Some(integral_u64(number).map_or_else(|| number.to_string(), |int| int.to_string()))
            }
        }
    }

    /// The value as an unsigned integer, accepting numeric strings and
    /// integral floats (`"12"`, `12.0`, `"12.0"`). `None` for negative,
    /// fractional, absent, or non-numeric values.
    fn to_u64(&self) -> Option<u64> {
        match self {
            Self::Unsigned(number) => Some(*number),
            Self::Float(number) => integral_u64(*number),
            Self::Text(text) => {
                let text = text.trim();
                text.parse()
                    .ok()
                    .or_else(|| text.parse().ok().and_then(integral_u64))
            }
            Self::Absent | Self::Signed(_) => None,
        }
    }
}

/// `number` as a `u64` when it is a whole number in range.
fn integral_u64(number: f64) -> Option<u64> {
    // 2^64, the first value past u64::MAX; exactly representable.
    const LIMIT: f64 = 18_446_744_073_709_551_616.0;
    // The checks make the cast exact: finite, whole, and in 0..2^64.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (number.is_finite() && number.fract() == 0.0 && (0.0..LIMIT).contains(&number))
        .then_some(number as u64)
}

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ScalarVisitor)
    }
}

/// Builds a [`Scalar`] from whatever JSON value comes next.
struct ScalarVisitor;

impl<'de> Visitor<'de> for ScalarVisitor {
    type Value = Scalar;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Scalar, E> {
        Ok(Scalar::Absent)
    }

    fn visit_i64<E: de::Error>(self, number: i64) -> Result<Scalar, E> {
        Ok(u64::try_from(number).map_or(Scalar::Signed(number), Scalar::Unsigned))
    }

    fn visit_u64<E: de::Error>(self, number: u64) -> Result<Scalar, E> {
        Ok(Scalar::Unsigned(number))
    }

    fn visit_f64<E: de::Error>(self, number: f64) -> Result<Scalar, E> {
        Ok(Scalar::Float(number))
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Scalar, E> {
        Ok(Scalar::Text(text.to_owned()))
    }

    fn visit_string<E: de::Error>(self, text: String) -> Result<Scalar, E> {
        Ok(Scalar::Text(text))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Scalar, E> {
        Ok(Scalar::Absent)
    }

    fn visit_none<E: de::Error>(self) -> Result<Scalar, E> {
        Ok(Scalar::Absent)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Scalar, D::Error> {
        Scalar::deserialize(deserializer)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Scalar, A::Error> {
        IgnoredAny.visit_seq(seq).map(|_| Scalar::Absent)
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Scalar, A::Error> {
        IgnoredAny.visit_map(map).map(|_| Scalar::Absent)
    }
}

/// Field-by-field accumulator for one player-API JSON object, turned into
/// its record type once the object is complete.
pub(super) trait RawRecord: Default {
    /// Identifies a field this record keeps.
    type Field: Copy;
    /// The finished record type.
    type Record;

    /// The field `key` names, or `None` for keys this record ignores.
    fn field(key: &str) -> Option<Self::Field>;

    /// Stores `value` for `field` (the last duplicate key wins).
    fn set(&mut self, field: Self::Field, value: Scalar);

    /// The finished record, or `None` when it is unusable. `key` is the
    /// object key the record was listed under, for lists sent as objects
    /// keyed by id; it stands in for a missing id.
    fn finish(self, key: Option<&str>) -> Option<Self::Record>;
}

/// Fields of a raw [`Category`].
#[derive(Debug, Clone, Copy)]
pub(super) enum CategoryField {
    /// `category_id`.
    Id,
    /// `category_name`.
    Name,
}

/// A [`Category`] as sent, every field optional.
#[derive(Debug, Default)]
pub(super) struct RawCategory {
    id: Scalar,
    name: Scalar,
}

impl RawRecord for RawCategory {
    type Field = CategoryField;
    type Record = Category;

    fn field(key: &str) -> Option<CategoryField> {
        match key {
            "category_id" => Some(CategoryField::Id),
            "category_name" => Some(CategoryField::Name),
            _ => None,
        }
    }

    fn set(&mut self, field: CategoryField, value: Scalar) {
        match field {
            CategoryField::Id => self.id = value,
            CategoryField::Name => self.name = value,
        }
    }

    /// A category needs an id (to be referenced by streams) and a name
    /// (its whole purpose: the group title). Without either it is skipped,
    /// and its streams are simply listed ungrouped.
    fn finish(self, key: Option<&str>) -> Option<Category> {
        let id = self.id.into_text().or_else(|| fallback_id(key))?;
        let name = self.name.into_text()?;
        Some(Category { id, name })
    }
}

/// Fields of a raw [`LiveStream`].
#[derive(Debug, Clone, Copy)]
pub(super) enum LiveStreamField {
    /// `name`.
    Name,
    /// `stream_id`.
    StreamId,
    /// `category_id`.
    CategoryId,
    /// `epg_channel_id`.
    EpgChannelId,
}

/// A [`LiveStream`] as sent, every field optional.
#[derive(Debug, Default)]
pub(super) struct RawLiveStream {
    name: Scalar,
    stream_id: Scalar,
    category_id: Scalar,
    epg_channel_id: Scalar,
}

impl RawRecord for RawLiveStream {
    type Field = LiveStreamField;
    type Record = LiveStream;

    fn field(key: &str) -> Option<LiveStreamField> {
        match key {
            "name" => Some(LiveStreamField::Name),
            "stream_id" => Some(LiveStreamField::StreamId),
            "category_id" => Some(LiveStreamField::CategoryId),
            "epg_channel_id" => Some(LiveStreamField::EpgChannelId),
            _ => None,
        }
    }

    fn set(&mut self, field: LiveStreamField, value: Scalar) {
        match field {
            LiveStreamField::Name => self.name = value,
            LiveStreamField::StreamId => self.stream_id = value,
            LiveStreamField::CategoryId => self.category_id = value,
            LiveStreamField::EpgChannelId => self.epg_channel_id = value,
        }
    }

    /// Only the stream id is required: without it there is no URL to
    /// play. Every other field defaults to "not set".
    fn finish(self, key: Option<&str>) -> Option<LiveStream> {
        let stream_id = self
            .stream_id
            .to_u64()
            .or_else(|| key.and_then(|key| Scalar::Text(key.to_owned()).to_u64()))?;
        Some(LiveStream {
            name: self.name.into_text(),
            stream_id,
            category_id: self.category_id.into_text(),
            epg_channel_id: self.epg_channel_id.into_text(),
        })
    }
}

/// An object key used as an id: trimmed, `None` when blank.
fn fallback_id(key: Option<&str>) -> Option<String> {
    let key = key?.trim();
    (!key.is_empty()).then(|| key.to_owned())
}

/// One list entry: `Some` raw record when it was a JSON object, `None`
/// for anything else (which is skipped, never an error).
struct Element<R>(Option<R>);

impl<'de, R: RawRecord> Deserialize<'de> for Element<R> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ElementVisitor(PhantomData))
    }
}

/// Builds an [`Element`]: objects field by field, everything else skipped.
struct ElementVisitor<R>(PhantomData<fn() -> R>);

impl<'de, R: RawRecord> Visitor<'de> for ElementVisitor<R> {
    type Value = Element<R>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Element<R>, A::Error> {
        let mut raw = R::default();
        while let Some(field) = map.next_key_seed(FieldSeed::<R>(PhantomData))? {
            match field {
                Some(field) => raw.set(field, map.next_value()?),
                None => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(Element(Some(raw)))
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Element<R>, E> {
        Ok(Element(None))
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Element<R>, E> {
        Ok(Element(None))
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Element<R>, E> {
        Ok(Element(None))
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Element<R>, E> {
        Ok(Element(None))
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<Element<R>, E> {
        Ok(Element(None))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Element<R>, E> {
        Ok(Element(None))
    }

    fn visit_none<E: de::Error>(self) -> Result<Element<R>, E> {
        Ok(Element(None))
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Element<R>, D::Error> {
        Element::deserialize(deserializer)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Element<R>, A::Error> {
        IgnoredAny.visit_seq(seq).map(|_| Element(None))
    }
}

/// Maps an object key to `R`'s field without allocating the key.
struct FieldSeed<R>(PhantomData<fn() -> R>);

impl<'de, R: RawRecord> DeserializeSeed<'de> for FieldSeed<R> {
    type Value = Option<R::Field>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl<R: RawRecord> Visitor<'_> for FieldSeed<R> {
    type Value = Option<R::Field>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a field name")
    }

    fn visit_str<E: de::Error>(self, key: &str) -> Result<Self::Value, E> {
        Ok(R::field(key))
    }
}

/// What a player-API reply turned out to be.
enum Outcome<T> {
    /// A list of records (possibly empty).
    List(ApiList<T>),
    /// The account object with `user_info.auth` reporting a rejection.
    AuthFailed,
    /// Valid JSON, but not a list.
    Unexpected,
}

/// Reads a whole player-API reply, one record at a time.
struct ListVisitor<R>(PhantomData<fn() -> R>);

impl<R: RawRecord> ListVisitor<R> {
    /// Adds one entry to `list`: finished if it is a usable record,
    /// counted as skipped otherwise.
    fn push(list: &mut ApiList<R::Record>, element: Element<R>, key: Option<&str>) {
        match element.0.and_then(|raw| raw.finish(key)) {
            Some(record) => list.records.push(record),
            None => list.skipped += 1,
        }
    }
}

impl<'de, R: RawRecord> Visitor<'de> for ListVisitor<R> {
    type Value = Outcome<R::Record>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON array or object")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut list = ApiList {
            records: Vec::with_capacity(seq.size_hint().unwrap_or(0)),
            skipped: 0,
        };
        while let Some(element) = seq.next_element::<Element<R>>()? {
            Self::push(&mut list, element, None);
        }
        Ok(Outcome::List(list))
    }

    /// An object is either the account-info reply (`user_info`, sent
    /// instead of a list when the credentials are wrong) or a list keyed
    /// by id. One with entries but no object values at all — such as
    /// `{"error":"maintenance"}` — is neither.
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut list = ApiList {
            records: Vec::new(),
            skipped: 0,
        };
        let mut user_info = None;
        let mut entries = 0_usize;
        let mut objects = 0_usize;
        while let Some(key) = map.next_key::<String>()? {
            entries += 1;
            if key == "user_info" {
                user_info = Some(map.next_value::<serde_json::Value>()?);
                continue;
            }
            let element = map.next_value::<Element<R>>()?;
            if element.0.is_some() {
                objects += 1;
            }
            Self::push(&mut list, element, Some(&key));
        }
        Ok(match user_info {
            Some(user_info) if auth_failed(&user_info) => Outcome::AuthFailed,
            Some(_) => Outcome::Unexpected,
            None if entries > 0 && objects == 0 => Outcome::Unexpected,
            None => Outcome::List(list),
        })
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Outcome::Unexpected)
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(Outcome::Unexpected)
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(Outcome::Unexpected)
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(Outcome::Unexpected)
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(Outcome::Unexpected)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(Outcome::Unexpected)
    }
}

#[cfg(test)]
// unwrap is fine in tests (see AGENTS.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn streams(json: &str) -> ApiList<LiveStream> {
        parse_api_list::<RawLiveStream>(json.as_bytes()).unwrap()
    }

    fn categories(json: &str) -> ApiList<Category> {
        parse_api_list::<RawCategory>(json.as_bytes()).unwrap()
    }

    fn ids(list: &ApiList<LiveStream>) -> Vec<u64> {
        list.records.iter().map(|stream| stream.stream_id).collect()
    }

    #[test]
    fn one_bad_stream_skips_that_stream_not_the_list() {
        // Regression: the list was deserialized all-or-nothing, so a
        // single record with a null id failed the whole player-API
        // fallback (XtreamError::Json) for a 55k-channel account.
        let list = streams(
            r#"[
                {"num":1,"name":"Good One","stream_type":"live","stream_id":101,
                 "stream_icon":"http://x/1.png","epg_channel_id":"one.tv","added":"1700000000",
                 "category_id":"5","custom_sid":null,"tv_archive":0,"direct_source":"",
                 "tv_archive_duration":0,"category_ids":[5]},
                {"num":2,"name":"Null Id","stream_id":null,"category_id":"5"},
                {"num":3,"name":"Empty Id","stream_id":"","category_id":"5"},
                {"num":4,"name":"Negative Id","stream_id":-1},
                {"num":5,"name":"Fractional Id","stream_id":"12.5"},
                {"num":6,"name":"Word Id","stream_id":"abc"},
                {"num":7,"name":"No Id"},
                "garbage",
                null,
                [1, 2, 3],
                {"num":8,"name":"Good Two","stream_id":"102","category_id":5}
            ]"#,
        );
        assert_eq!(ids(&list), [101, 102]);
        assert_eq!(list.skipped, 9);
        assert_eq!(list.records[0].epg_channel_id.as_deref(), Some("one.tv"));
        assert_eq!(list.records[1].category_id.as_deref(), Some("5"));
    }

    #[test]
    fn stream_ids_accept_numeric_strings_and_integral_floats() {
        let list = streams(
            r#"[
                {"stream_id":"12.0"},{"stream_id":13.0},{"stream_id":" 14 "},
                {"stream_id":"1.5e1"},{"stream_id":18446744073709551615}
            ]"#,
        );
        assert_eq!(ids(&list), [12, 13, 14, 15, u64::MAX]);
        assert_eq!(list.skipped, 0);
    }

    #[test]
    fn null_and_odd_optional_fields_default_to_unset() {
        let list = streams(
            r#"[
                {"name":null,"stream_id":1,"category_id":null,"epg_channel_id":null},
                {"name":"  ","stream_id":2,"category_id":"","epg_channel_id":false},
                {"name":{"en":"x"},"stream_id":3,"category_id":[7],"epg_channel_id":[]},
                {"name":42,"stream_id":4,"category_id":7.0,"epg_channel_id":"  four.tv "}
            ]"#,
        );
        assert_eq!(list.skipped, 0);
        for stream in &list.records[..3] {
            assert_eq!(stream.name, None);
            assert_eq!(stream.category_id, None);
            assert_eq!(stream.epg_channel_id, None);
        }
        let numeric = &list.records[3];
        assert_eq!(numeric.name.as_deref(), Some("42"));
        assert_eq!(numeric.category_id.as_deref(), Some("7"));
        assert_eq!(numeric.epg_channel_id.as_deref(), Some("four.tv"));
    }

    #[test]
    fn duplicate_keys_do_not_fail_the_record() {
        let list = streams(r#"[{"stream_id":1,"name":"Old","name":"New"}]"#);
        assert_eq!(list.records[0].name.as_deref(), Some("New"));
    }

    #[test]
    fn categories_tolerate_null_names_and_ids() {
        // Regression: `category_name: String` rejected null, failing the
        // whole category list over one entry.
        let list = categories(
            r#"[
                {"category_id":"1","category_name":"News","parent_id":0},
                {"category_id":"2","category_name":null,"parent_id":0},
                {"category_id":null,"category_name":"Orphan"},
                {"category_id":"","category_name":"Blank"},
                {"category_id":4.0,"category_name":"Sports"},
                {"category_id":5,"category_name":12}
            ]"#,
        );
        let pairs: Vec<(&str, &str)> = list
            .records
            .iter()
            .map(|c| (c.id.as_str(), c.name.as_str()))
            .collect();
        assert_eq!(pairs, [("1", "News"), ("4", "Sports"), ("5", "12")]);
        assert_eq!(list.skipped, 3);
    }

    #[test]
    fn lists_keyed_by_id_are_accepted() {
        let list = streams(
            r#"{
                "101":{"name":"One","stream_id":101},
                "102":{"name":"Two"},
                "x":{"name":"Unusable"},
                "103":"junk"
            }"#,
        );
        // The key stands in for a missing stream id.
        assert_eq!(ids(&list), [101, 102]);
        assert_eq!(list.skipped, 2);

        let list = categories(
            r#"{"7":{"category_name":"Movies"},"8":{"category_id":"8","category_name":"Kids"}}"#,
        );
        let pairs: Vec<(&str, &str)> = list
            .records
            .iter()
            .map(|c| (c.id.as_str(), c.name.as_str()))
            .collect();
        assert_eq!(pairs, [("7", "Movies"), ("8", "Kids")]);
    }

    #[test]
    fn empty_lists_in_either_shape_are_empty_not_errors() {
        for json in ["[]", "{}", " [ ] "] {
            let list = streams(json);
            assert!(list.records.is_empty(), "{json}");
            assert_eq!(list.skipped, 0, "{json}");
        }
    }

    #[test]
    fn non_list_replies_are_unexpected_or_auth_failures() {
        for json in [
            r#"{"error":"maintenance"}"#,
            r#"{"user_info":{"auth":1},"server_info":{"url":"x"}}"#,
            "null",
            "\"nope\"",
            "42",
        ] {
            let error = parse_api_list::<RawLiveStream>(json.as_bytes()).unwrap_err();
            assert!(matches!(error, XtreamError::UnexpectedApiReply), "{json}");
        }
        let error =
            parse_api_list::<RawLiveStream>(br#"{"user_info":{"auth":0}}"#.as_slice()).unwrap_err();
        assert!(matches!(error, XtreamError::AuthFailed));
    }

    #[test]
    fn large_list_streams_through_with_scattered_bad_records() {
        use std::fmt::Write as _;

        // Generated, not checked in (AGENTS.md): a 55k-stream reply of the
        // size that used to be buffered as a whole `Value` tree first.
        const COUNT: u64 = 55_000;
        let mut json = String::from("[");
        for id in 1..=COUNT {
            if id > 1 {
                json.push(',');
            }
            let stream_id = if id % 1000 == 0 {
                "null".to_owned()
            } else {
                format!("\"{id}\"")
            };
            let category = id % 40;
            write!(
                json,
                r#"{{"num":{id},"name":"Channel {id}","stream_type":"live","stream_id":{stream_id},"stream_icon":"http://logo/{id}.png","epg_channel_id":"c{id}.tv","added":"1700000000","category_id":"{category}","tv_archive":0,"direct_source":"","category_ids":[{category}]}}"#,
            )
            .unwrap();
        }
        json.push(']');
        let list = streams(&json);
        assert_eq!(list.skipped, 55);
        assert_eq!(list.records.len(), 54_945);
        let final_record = list.records.last().unwrap();
        assert_eq!(final_record.stream_id, COUNT - 1);
        assert_eq!(final_record.name.as_deref(), Some("Channel 54999"));
    }

    #[test]
    fn trailing_garbage_after_the_list_is_an_error() {
        let error =
            parse_api_list::<RawLiveStream>(br#"[{"stream_id":1}] x"#.as_slice()).unwrap_err();
        assert!(matches!(error, XtreamError::Json(_)));
    }

    #[test]
    fn malformed_json_is_still_an_error() {
        let error =
            parse_api_list::<RawLiveStream>(br#"[{"stream_id":1},"#.as_slice()).unwrap_err();
        assert!(matches!(error, XtreamError::Json(_)));
    }

    #[test]
    fn auth_failure_accepts_common_panel_scalar_types() {
        for auth in [
            serde_json::Value::Bool(false),
            serde_json::Value::Number(0.into()),
            serde_json::Value::String("0".into()),
        ] {
            assert!(auth_failed(&serde_json::json!({ "auth": auth })));
        }
        for auth in [
            serde_json::Value::Bool(true),
            serde_json::Value::Number(1.into()),
            serde_json::Value::String("1".into()),
        ] {
            assert!(!auth_failed(&serde_json::json!({ "auth": auth })));
        }
    }
}
