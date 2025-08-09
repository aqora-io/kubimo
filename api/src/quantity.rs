use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

pub use k8s_openapi::apimachinery::pkg::api::resource::Quantity as KubeQuantity;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString};

pub type StorageQuantity = Quantity<StorageUnit>;
pub type CpuQuantity = Quantity<CpuUnit>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Display, EnumString, JsonSchema)]
pub enum StorageUnit {
    #[strum(serialize = "")]
    B,
    Ki,
    Mi,
    Gi,
    Ti,
    Pi,
    Ei,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Display, EnumString, JsonSchema)]
pub enum CpuUnit {
    #[strum(serialize = "")]
    Core,
    #[strum(serialize = "m")]
    Milli,
}

#[derive(Deserialize, Serialize)]
#[serde(from = "KubeQuantity", into = "KubeQuantity")]
pub struct Quantity<T> {
    quantity: KubeQuantity,
    marker: PhantomData<T>,
}

impl<T> JsonSchema for Quantity<T> {
    fn schema_name() -> String {
        KubeQuantity::schema_name()
    }
    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        KubeQuantity::json_schema(generator)
    }
    fn is_referenceable() -> bool {
        KubeQuantity::is_referenceable()
    }
    fn schema_id() -> Cow<'static, str> {
        KubeQuantity::schema_id()
    }
}

impl<T> Default for Quantity<T> {
    fn default() -> Self {
        Self {
            quantity: KubeQuantity::default(),
            marker: PhantomData,
        }
    }
}

impl<T> Clone for Quantity<T> {
    fn clone(&self) -> Self {
        Self {
            quantity: self.quantity.clone(),
            marker: PhantomData,
        }
    }
}

impl<T> Quantity<T> {
    pub fn new(value: impl Into<f64>, unit: T) -> Self
    where
        T: fmt::Display,
    {
        Quantity {
            quantity: KubeQuantity(format!("{}{}", value.into(), unit)),
            marker: PhantomData,
        }
    }

    pub fn as_unit(&self) -> Option<(f64, T)>
    where
        T: FromStr,
    {
        let split = self.quantity.0.find(|c: char| c.is_alphabetic())?;
        let value = self.quantity.0[..split].parse::<f64>().ok()?;
        let unit = self.quantity.0[split..].parse::<T>().ok()?;
        Some((value, unit))
    }
}

impl<T> From<KubeQuantity> for Quantity<T> {
    fn from(quantity: KubeQuantity) -> Self {
        Self {
            quantity,
            marker: PhantomData,
        }
    }
}

impl<T> From<Quantity<T>> for KubeQuantity {
    fn from(quantity: Quantity<T>) -> Self {
        quantity.quantity
    }
}

impl<T, U> From<(T, U)> for Quantity<U>
where
    T: Into<f64>,
    U: fmt::Display,
{
    fn from((value, unit): (T, U)) -> Self {
        Quantity::new(value, unit)
    }
}

impl<T> fmt::Debug for Quantity<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Quantity").field(&self.quantity.0).finish()
    }
}

impl<T> fmt::Display for Quantity<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.quantity.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_storage_quantity() {
        let sq = Quantity::new(10, StorageUnit::Gi);
        assert_eq!(sq.to_string(), "10Gi");
        assert_eq!(sq.as_unit(), Some((10.0, StorageUnit::Gi)));

        let sq_from_quantity: Quantity<StorageUnit> = KubeQuantity("20Mi".to_string()).into();
        assert_eq!(sq_from_quantity.to_string(), "20Mi");
        assert_eq!(sq_from_quantity.as_unit(), Some((20.0, StorageUnit::Mi)));
    }
}
