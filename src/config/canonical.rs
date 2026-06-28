use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use toml::Value;

use crate::hex::push_hex_byte;

#[derive(Debug, Default)]
pub(crate) struct CanonicalWriter {
    output: String,
}

impl CanonicalWriter {
    pub(crate) fn finish(self) -> String {
        self.output
    }

    pub(crate) fn object<F>(&mut self, name: &str, write_fields: F)
    where
        F: FnOnce(&mut Self),
    {
        self.output.push_str("object");
        self.string(name);
        self.output.push('[');
        write_fields(self);
        self.output.push(']');
    }

    pub(crate) fn field<F>(&mut self, name: &str, write_value: F)
    where
        F: FnOnce(&mut Self),
    {
        self.output.push_str("field");
        self.string(name);
        self.output.push('=');
        write_value(self);
        self.output.push(';');
    }

    pub(crate) fn seq<'a, T, I, F>(&mut self, values: I, mut write_value: F)
    where
        I: IntoIterator<Item = &'a T>,
        T: 'a,
        F: FnMut(&mut Self, &'a T),
    {
        self.output.push_str("seq[");
        for value in values {
            write_value(self, value);
            self.output.push(';');
        }
        self.output.push(']');
    }

    pub(crate) fn map<'a, V, I, F>(&mut self, entries: I, mut write_value: F)
    where
        I: IntoIterator<Item = (&'a String, &'a V)>,
        V: 'a,
        F: FnMut(&mut Self, &'a V),
    {
        let mut entries = entries.into_iter().collect::<Vec<_>>();
        entries.sort_by_key(|(key, _)| *key);

        self.output.push_str("map{");
        for (key, value) in entries {
            self.string(key);
            self.output.push('=');
            write_value(self, value);
            self.output.push(';');
        }
        self.output.push('}');
    }

    pub(crate) fn option_string(&mut self, value: Option<&str>) {
        match value {
            Some(value) => self.string(value),
            None => self.none(),
        }
    }

    pub(crate) fn string(&mut self, value: &str) {
        self.output.push('s');
        self.output.push_str(&value.len().to_string());
        self.output.push(':');
        self.output.push_str(value);
    }

    pub(crate) fn bool(&mut self, value: bool) {
        if value {
            self.output.push_str("b1");
        } else {
            self.output.push_str("b0");
        }
    }

    fn i64(&mut self, value: i64) {
        self.output.push('i');
        self.output.push_str(&value.to_string());
    }

    fn f64(&mut self, value: f64) {
        self.output.push('f');
        push_hex_u64(&mut self.output, value.to_bits());
    }

    pub(crate) fn none(&mut self) {
        self.output.push_str("none");
    }

    pub(crate) fn toml_value(&mut self, value: &Value) {
        match value {
            Value::String(value) => {
                self.output.push_str("toml-string");
                self.string(value);
            }
            Value::Integer(value) => {
                self.output.push_str("toml-integer");
                self.i64(*value);
            }
            Value::Float(value) => {
                self.output.push_str("toml-float");
                self.f64(*value);
            }
            Value::Boolean(value) => {
                self.output.push_str("toml-boolean");
                self.bool(*value);
            }
            Value::Datetime(value) => {
                self.output.push_str("toml-datetime");
                self.string(&value.to_string());
            }
            Value::Array(values) => {
                self.output.push_str("toml-array");
                self.seq(values.iter(), Self::toml_value);
            }
            Value::Table(values) => {
                self.output.push_str("toml-table");
                self.map(values.iter(), Self::toml_value);
            }
        }
    }

    pub(crate) fn json_value(&mut self, value: &JsonValue) {
        match value {
            JsonValue::Null => self.output.push_str("json-null"),
            JsonValue::Bool(value) => {
                self.output.push_str("json-bool");
                self.bool(*value);
            }
            JsonValue::Number(value) => {
                self.output.push_str("json-number");
                self.string(&value.to_string());
            }
            JsonValue::String(value) => {
                self.output.push_str("json-string");
                self.string(value);
            }
            JsonValue::Array(values) => {
                self.output.push_str("json-array");
                self.seq(values.iter(), Self::json_value);
            }
            JsonValue::Object(values) => {
                self.output.push_str("json-object");
                self.map(values.iter(), Self::json_value);
            }
        }
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);

    for byte in digest {
        push_hex_byte(&mut output, byte);
    }

    output
}

fn push_hex_u64(output: &mut String, value: u64) {
    for byte in value.to_be_bytes() {
        push_hex_byte(output, byte);
    }
}
