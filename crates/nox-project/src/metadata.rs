//! Free-form metadata, as found in OTIO's `metadata` dictionaries.
//!
//! It is carried through unchanged so that data written by other tools
//! survives a round trip through Nox. Unlike `serde_json::Value` it can hold
//! non-finite floats, which OTIO files may contain.

/// A JSON-like value.
#[derive(Debug, Clone, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    /// Integers above `i64::MAX`.
    UInt(u64),
    Float(f64),
    String(String),
    Array(Vec<Value>),
    Object(Object),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Any number as `f64`.
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Self::Int(i) => Some(i as f64),
            Self::UInt(u) => Some(u as f64),
            Self::Float(f) => Some(f),
            _ => None,
        }
    }

    /// Integers, and floats with an integral value that fits.
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Self::Int(i) => Some(i),
            Self::UInt(u) => i64::try_from(u).ok(),
            Self::Float(f) if f.fract() == 0.0 && f.abs() < i64::MAX as f64 => Some(f as i64),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Self::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Object> {
        match self {
            Self::Object(o) => Some(o),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

/// Floats compare by value except that NaN equals NaN, so that documents
/// can be compared after a round trip.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::UInt(a), Self::UInt(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a == b || (a.is_nan() && b.is_nan()),
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Array(a), Self::Array(b)) => a == b,
            (Self::Object(a), Self::Object(b)) => a == b,
            _ => false,
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Self::String(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Self::Int(i)
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Self::Float(f)
    }
}

impl From<Object> for Value {
    fn from(o: Object) -> Self {
        Self::Object(o)
    }
}

/// String keys to values, in insertion order. Comparison ignores order.
#[derive(Debug, Clone, Default)]
pub struct Object(Vec<(String, Value)>);

impl Object {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.0.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Sets `key`, keeping its position if it already exists.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        let (key, value) = (key.into(), value.into());
        match self.get_mut(&key) {
            Some(slot) => *slot = value,
            None => self.0.push((key, value)),
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let index = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.remove(index).1)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }
}

impl PartialEq for Object {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|(k, v)| other.get(k) == Some(v))
    }
}

impl<K: Into<String>, V: Into<Value>> FromIterator<(K, V)> for Object {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut object = Self::new();
        for (k, v) in iter {
            object.insert(k, v);
        }
        object
    }
}

impl IntoIterator for Object {
    type Item = (String, Value);
    type IntoIter = std::vec::IntoIter<(String, Value)>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_keeps_order_and_compares_unordered() {
        let mut a = Object::new();
        a.insert("b", 1);
        a.insert("a", 2);
        a.insert("b", 3);
        assert_eq!(a.iter().map(|(k, _)| k).collect::<Vec<_>>(), ["b", "a"]);
        let b: Object = [("a", 2), ("b", 3)].into_iter().collect();
        assert_eq!(a, b);
        assert_eq!(a.remove("a"), Some(Value::Int(2)));
        assert_ne!(a, b);
    }

    #[test]
    fn nan_equals_nan() {
        assert_eq!(Value::Float(f64::NAN), Value::Float(f64::NAN));
        assert_ne!(Value::Float(1.0), Value::Int(1));
    }
}
