#![allow(dead_code)]
use std::str::FromStr;

use num::rational::Rational64 as Rational;
use num::traits::{CheckedMul, Inv, Signed};
use num::{FromPrimitive, One};
use uom::si::frequency::hertz;
use uom::si::rational64::{Frequency as UOM_Frequency, Time as UOM_Time};
use uom::si::time::second;

use super::ast::{ExprNode, ExprVariant, LiteralKind, Shift, TimeUnit, TokenLiteral};
use crate::parse::OORVSpecParser;

pub(crate) type RationalType = i64;

impl ExprNode {
    pub(crate) fn parse_duration(&self) -> Result<UOM_Time, String> {
        let (rational_val, unit_str) = self.numeric_with_unit()?;
        let scale: UOM_Time = time_unit_to_uom(unit_str)?;
        let factor = scale.get::<second>();
        rational_val
            .checked_mul(&factor)
            .map(|d| UOM_Time::new::<second>(d))
            .ok_or_else(|| format!("overflow when parsing duration `{}`", self))
    }

    pub(crate) fn parse_frequency(&self) -> Result<UOM_Frequency, String> {
        let (rational_val, unit_str) = self.numeric_with_unit()?;
        if !rational_val.is_positive() {
            return Err("frequencies must be positive".to_string());
        }
        let scale: UOM_Frequency = freq_unit_to_uom(unit_str)?;
        let factor = scale.get::<hertz>();
        rational_val
            .checked_mul(&factor)
            .map(|f| UOM_Frequency::new::<hertz>(f))
            .ok_or_else(|| format!("overflow when parsing frequency `{}`", self))
    }

    pub fn parse_freqspec(&self) -> Result<UOM_Frequency, String> {
        if let Ok(freq) = self.parse_frequency() {
            return Ok(freq);
        }
        let period = self.parse_duration()?;
        let secs = period.get::<second>();
        if secs.is_positive() {
            Ok(UOM_Frequency::new::<hertz>(secs.inv()))
        } else {
            Err(format!("period must be positive, found {:?}", period))
        }
    }

    pub fn parse_discrete_duration(&self) -> Result<u64, String> {
        match &self.kind {
            ExprVariant::Literal(lit) => match &lit.kind {
                LiteralKind::Number(raw, None) => raw
                    .parse()
                    .map_err(|e: std::num::ParseIntError| e.to_string()),
                _ => Err(format!("expected unit-less integer, found {}", lit)),
            },
            _ => Err(format!("expected unit-less integer, found {}", self)),
        }
    }

    fn numeric_with_unit(&self) -> Result<(Rational, &str), String> {
        match &self.kind {
            ExprVariant::Literal(lit) => match &lit.kind {
                LiteralKind::Number(raw, Some(unit)) => {
                    let r = OORVSpecParser::parse_rational(raw)?;
                    Ok((r, unit.as_str()))
                }
                _ => Err(format!("expected numeric value with unit, found {}", lit)),
            },
            _ => Err(format!("expected numeric value with unit, found {}", self)),
        }
    }
}

impl TokenLiteral {
    pub(crate) fn parse_numeric<T: FromStr>(&self) -> Option<T> {
        if let LiteralKind::Number(raw, None) = &self.kind {
            raw.parse::<T>().ok()
        } else {
            None
        }
    }
}

impl Shift {
    pub fn to_uom_time(&self) -> Option<UOM_Time> {
        match self {
            Shift::Discrete(_) => None,
        }
    }
}

impl FromStr for TimeUnit {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let unit = match s {
            "ns" => TimeUnit::Nanosecond,
            "us" => TimeUnit::Microsecond,
            "ms" => TimeUnit::Millisecond,
            "s" => TimeUnit::Second,
            "min" => TimeUnit::Minute,
            "h" => TimeUnit::Hour,
            "d" => TimeUnit::Day,
            "w" => TimeUnit::Week,
            "a" => TimeUnit::Year,
            other => return Err(format!("unknown time unit {}", other)),
        };
        Ok(unit)
    }
}

impl TimeUnit {
    pub(crate) fn to_uom_time(self) -> UOM_Time {
        use uom::si::time::*;
        let ratio = match self {
            TimeUnit::Nanosecond => Rational::new(1_i64, 1_000_000_000_i64),
            TimeUnit::Microsecond => Rational::new(1_i64, 1_000_000_i64),
            TimeUnit::Millisecond => Rational::new(1_i64, 1_000_i64),
            TimeUnit::Second => Rational::from_u64(1).unwrap(),
            TimeUnit::Minute => Rational::from_u64(60).unwrap(),
            TimeUnit::Hour => Rational::from_u64(3600).unwrap(),
            TimeUnit::Day => Rational::from_u64(86_400).unwrap(),
            TimeUnit::Week => Rational::from_u64(604_800).unwrap(),
            TimeUnit::Year => Rational::from_u64(31_536_000).unwrap(),
        };
        UOM_Time::new::<second>(ratio)
    }
}

fn time_unit_to_uom(unit: &str) -> Result<UOM_Time, String> {
    use uom::si::time::*;
    let t = match unit {
        "ns" => UOM_Time::new::<nanosecond>(Rational::one()),
        "us" => UOM_Time::new::<microsecond>(Rational::one()),
        "ms" => UOM_Time::new::<millisecond>(Rational::one()),
        "s" => UOM_Time::new::<second>(Rational::one()),
        "min" => UOM_Time::new::<minute>(Rational::one()),
        "h" => UOM_Time::new::<hour>(Rational::one()),
        "d" => UOM_Time::new::<day>(Rational::one()),
        "w" => UOM_Time::new::<day>(Rational::from_u64(7).unwrap()),
        "a" => UOM_Time::new::<day>(Rational::from_u64(365).unwrap()),
        other => return Err(format!("expected duration unit, found {}", other)),
    };
    Ok(t)
}

fn freq_unit_to_uom(unit: &str) -> Result<UOM_Frequency, String> {
    use uom::si::frequency::*;
    let f = match unit {
        "uHz" => UOM_Frequency::new::<microhertz>(Rational::one()),
        "mHz" => UOM_Frequency::new::<millihertz>(Rational::one()),
        "Hz" => UOM_Frequency::new::<hertz>(Rational::one()),
        "kHz" => UOM_Frequency::new::<kilohertz>(Rational::one()),
        "MHz" => UOM_Frequency::new::<megahertz>(Rational::one()),
        "GHz" => UOM_Frequency::new::<gigahertz>(Rational::one()),
        other => return Err(format!("expected frequency unit, found {}", other)),
    };
    Ok(f)
}
