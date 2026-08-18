use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq)]
pub enum Parameter {
    Float(f32),
    Int(i32),
    Bool(bool),
}

#[derive(Debug)]
pub enum ParameterError {
    TypeMismatch,
}

// Numeric parameters accept either JSON number kind. A hand written config (or one pasted from
// somewhere else) writes `20` where the algorithm expects a float, and serde has no way to know
// the difference; reading it as a hard type mismatch used to panic mid-analysis.
impl TryFrom<&Parameter> for f32 {
    type Error = ParameterError;

    fn try_from(param: &Parameter) -> Result<Self, Self::Error> {
        match param {
            Parameter::Float(f) => Ok(*f),
            Parameter::Int(i) => Ok(*i as f32),
            Parameter::Bool(_) => Err(ParameterError::TypeMismatch),
        }
    }
}

impl TryFrom<&Parameter> for i32 {
    type Error = ParameterError;

    fn try_from(param: &Parameter) -> Result<Self, Self::Error> {
        match param {
            Parameter::Int(i) => Ok(*i),
            Parameter::Float(f) => Ok(f.round() as i32),
            Parameter::Bool(_) => Err(ParameterError::TypeMismatch),
        }
    }
}

impl TryFrom<&Parameter> for bool {
    type Error = ParameterError;

    fn try_from(param: &Parameter) -> Result<Self, Self::Error> {
        if let Parameter::Bool(b) = param {
            Ok(*b)
        } else {
            Err(ParameterError::TypeMismatch)
        }
    }
}

impl TryFrom<&Parameter> for Parameter {
    type Error = ParameterError;

    fn try_from(param: &Parameter) -> Result<Self, Self::Error> {
        Ok(param.clone())
    }
}

impl Parameter {
    // The JSON shape this parameter is stored as. Two parameters with the same kind can be
    // swapped without changing how a widget or an algorithm reads them.
    pub fn kind(&self) -> ParameterKind {
        match self {
            Parameter::Float(_) => ParameterKind::Float,
            Parameter::Int(_) => ParameterKind::Int,
            Parameter::Bool(_) => ParameterKind::Bool,
        }
    }

    // Reshapes a value read from a config file into the kind the algorithm actually declares.
    // `20` pasted for a float parameter becomes `Float(20.0)` rather than an `Int` that every
    // later reader has to second-guess. Returns `None` when the two can't be reconciled
    // (a bool where a number belongs, or the other way round).
    pub fn coerced_like(&self, template: &Parameter) -> Option<Parameter> {
        match (self, template.kind()) {
            (Parameter::Float(f), ParameterKind::Float) => Some(Parameter::Float(*f)),
            (Parameter::Int(i), ParameterKind::Float) => Some(Parameter::Float(*i as f32)),
            (Parameter::Int(i), ParameterKind::Int) => Some(Parameter::Int(*i)),
            (Parameter::Float(f), ParameterKind::Int) => Some(Parameter::Int(f.round() as i32)),
            (Parameter::Bool(b), ParameterKind::Bool) => Some(Parameter::Bool(*b)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterKind {
    Float,
    Int,
    Bool,
}

impl Clone for Parameter {
    fn clone(&self) -> Self {
        match self {
            Parameter::Float(f) => Parameter::Float(*f),
            Parameter::Int(i) => Parameter::Int(*i),
            Parameter::Bool(b) => Parameter::Bool(*b),
        }
    }
}

impl Serialize for Parameter {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Parameter::Float(f) => serializer.serialize_f32(*f),
            Parameter::Int(i) => serializer.serialize_i32(*i),
            Parameter::Bool(b) => serializer.serialize_bool(*b),
        }
    }
}

impl<'a> Deserialize<'a> for Parameter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'a>,
    {
        struct ParameterVisitor;

        impl<'de> serde::de::Visitor<'de> for ParameterVisitor {
            type Value = Parameter;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("an f32, i32, or bool")
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Parameter::Float(value as f32))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Parameter::Int(value as i32))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Parameter::Int(value as i32))
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Parameter::Bool(value))
            }
        }

        deserializer.deserialize_any(ParameterVisitor)
    }
}

// Maps parameter names to their values.
pub type Parameters = HashMap<String, Parameter>;

// Maps algorithm names to their parameters.
pub type Config = HashMap<String, Parameters>;

pub fn get_parameter_value<T>(params: &Parameters, param_name: &str) -> T
where
    T: for<'a> TryFrom<&'a Parameter, Error = ParameterError>,
{
    match params.get(param_name) {
        Some(param) => match T::try_from(param) {
            Ok(value) => value,
            Err(_) => panic!("Parameter {} has wrong type", param_name),
        },
        None => panic!("Parameter {} not found", param_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A config written by hand says `20`, not `20.0`, and serde has no way to tell that apart from
    // an int parameter. Both number kinds have to read back as the number that was written.
    #[test]
    fn numbers_read_back_regardless_of_json_kind() {
        assert_eq!(f32::try_from(&Parameter::Int(20)).unwrap(), 20.0);
        assert_eq!(f32::try_from(&Parameter::Float(20.5)).unwrap(), 20.5);
        assert_eq!(i32::try_from(&Parameter::Float(20.4)).unwrap(), 20);
        assert_eq!(i32::try_from(&Parameter::Int(20)).unwrap(), 20);
        assert!(f32::try_from(&Parameter::Bool(true)).is_err());
        assert!(i32::try_from(&Parameter::Bool(true)).is_err());
    }

    #[test]
    fn coercion_follows_the_template_not_the_value() {
        let float_param = Parameter::Float(1.0);
        let int_param = Parameter::Int(1);
        let bool_param = Parameter::Bool(false);

        assert_eq!(
            Parameter::Int(20).coerced_like(&float_param),
            Some(Parameter::Float(20.0))
        );
        assert_eq!(
            Parameter::Float(20.6).coerced_like(&int_param),
            Some(Parameter::Int(21))
        );
        assert_eq!(
            Parameter::Bool(true).coerced_like(&bool_param),
            Some(Parameter::Bool(true))
        );
        assert_eq!(Parameter::Bool(true).coerced_like(&float_param), None);
        assert_eq!(Parameter::Float(1.0).coerced_like(&bool_param), None);
    }

    #[test]
    fn whole_numbers_deserialize_as_ints() {
        // Documents the ambiguity the coercion exists to paper over.
        let parsed: Parameters = serde_json::from_str(r#"{"a": 20, "b": 20.0}"#).unwrap();
        assert_eq!(parsed["a"], Parameter::Int(20));
        assert_eq!(parsed["b"], Parameter::Float(20.0));
    }
}
