use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::diagnostics::Reading;

macro_rules! choice {
    ($name:ident, $first:ident => $label:literal $(, $variant:ident => $other:literal)* $(,)?) => {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { #[default] $first, $($variant,)* }
        impl $name {
            pub const ALL: &'static [Self] = &[Self::$first, $(Self::$variant,)*];
            pub fn label(self) -> &'static str {
                match self { Self::$first => $label, $(Self::$variant => $other,)* }
            }
        }
    };
}

choice!(Speed, Kmh => "km/h", Mph => "mph");
choice!(MassFlow, GramsPerSecond => "g/s", KilogramsPerHour => "kg/h", PoundsPerMinute => "lb/min");
choice!(Temperature, Celsius => "°C", Fahrenheit => "°F");
choice!(Pressure, Kpa => "kPa", Psi => "psi", Bar => "bar");
choice!(Distance, Kilometers => "km", Miles => "mi");

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Units {
    pub speed: Speed,
    pub mass_flow: MassFlow,
    pub temperature: Temperature,
    pub pressure: Pressure,
    pub distance: Distance,
}

#[derive(Clone, Copy)]
pub(super) struct Conversion {
    scale: f64,
    offset: f64,
    unit: Option<&'static str>,
}

impl Conversion {
    pub(super) fn apply(self, value: f64) -> Option<f64> {
        let value = value * self.scale + self.offset;
        value.is_finite().then_some(value)
    }

    pub(super) fn unit(self, original: &str) -> &str {
        self.unit.unwrap_or(original)
    }
}

impl Units {
    pub(super) fn summary(self) -> String {
        format!(
            "{} · {} · {} · {} · {}",
            self.speed.label(),
            self.mass_flow.label(),
            self.temperature.label(),
            self.pressure.label(),
            self.distance.label()
        )
    }

    pub(super) fn conversion(self, unit: &str) -> Conversion {
        let linear = |source: f64, target: f64, unit| Conversion {
            scale: source / target,
            offset: 0.0,
            unit: Some(unit),
        };
        match unit {
            "kilometersPerHour" | "km/h" | "kph" | "milesPerHour" | "mph" => {
                let source = if matches!(unit, "milesPerHour" | "mph") {
                    1.609344
                } else {
                    1.0
                };
                linear(
                    source,
                    if self.speed == Speed::Mph {
                        1.609344
                    } else {
                        1.0
                    },
                    self.speed.label(),
                )
            }
            "gramsPerSecond" | "g/s" | "kg/h" | "kg/s" | "lb/min" | "poundsPerMinute" => {
                let source = match unit {
                    "kg/h" => 1.0 / 3.6,
                    "kg/s" => 1000.0,
                    "lb/min" | "poundsPerMinute" => 453.59237 / 60.0,
                    _ => 1.0,
                };
                let target = match self.mass_flow {
                    MassFlow::GramsPerSecond => 1.0,
                    MassFlow::KilogramsPerHour => 1.0 / 3.6,
                    MassFlow::PoundsPerMinute => 453.59237 / 60.0,
                };
                linear(source, target, self.mass_flow.label())
            }
            "celsius" | "C" | "°C" | "fahrenheit" | "F" | "°F" => {
                let (scale, offset) = if matches!(unit, "fahrenheit" | "F" | "°F") {
                    (5.0 / 9.0, -32.0 * 5.0 / 9.0)
                } else {
                    (1.0, 0.0)
                };
                if self.temperature == Temperature::Fahrenheit {
                    Conversion {
                        scale: scale * 1.8,
                        offset: offset * 1.8 + 32.0,
                        unit: Some("°F"),
                    }
                } else {
                    Conversion {
                        scale,
                        offset,
                        unit: Some("°C"),
                    }
                }
            }
            "kilopascal" | "kPa" | "Pa" | "bar" | "psi" | "poundsPerSquareInch" => {
                let source = match unit {
                    "Pa" => 0.001,
                    "bar" => 100.0,
                    "psi" | "poundsPerSquareInch" => 6.894_757_293_168,
                    _ => 1.0,
                };
                let target = match self.pressure {
                    Pressure::Kpa => 1.0,
                    Pressure::Psi => 6.894_757_293_168,
                    Pressure::Bar => 100.0,
                };
                linear(source, target, self.pressure.label())
            }
            "kilometers" | "km" | "meters" | "m" | "miles" | "mi" => {
                let source = match unit {
                    "meters" | "m" => 0.001,
                    "miles" | "mi" => 1.609344,
                    _ => 1.0,
                };
                linear(
                    source,
                    if self.distance == Distance::Miles {
                        1.609344
                    } else {
                        1.0
                    },
                    self.distance.label(),
                )
            }
            _ => Conversion {
                scale: 1.0,
                offset: 0.0,
                unit: None,
            },
        }
    }

    pub(super) fn reading<'a>(self, reading: &'a Reading) -> Cow<'a, Reading> {
        let conversion = self.conversion(&reading.unit);
        // Structured monitor results keep their values and limit units together.
        if reading.request.starts_with("05")
            || reading.request.starts_with("06")
            || conversion.unit.is_none()
            || conversion.scale == 1.0 && conversion.offset == 0.0
            || reading.numeric.is_some() && reading.value.parse::<f64>().is_err()
        {
            return Cow::Borrowed(reading);
        }
        let mut display = reading.clone();
        display.unit = conversion.unit(&reading.unit).to_owned();
        if let Some(number) = reading.numeric {
            if let Some(number) = conversion.apply(number) {
                display.numeric = Some(number);
                display.value = format_number(number);
            } else {
                display.numeric = None;
                display.value = "Outside display range".into();
            }
        }
        Cow::Owned(display)
    }

    pub(super) fn snapshot(self, snapshot: &super::model::Snapshot) -> super::model::Snapshot {
        let mut display = snapshot.clone();
        for row in display.rows.iter_mut().chain(&mut display.failures) {
            row.reading = self.reading(&row.reading).into_owned();
        }
        display
    }
}

pub(super) fn format_number(number: f64) -> String {
    let rounded = format!("{number:.2}");
    if number != 0.0 && rounded.parse() == Ok(0.0) {
        format!("{number:.6e}")
    } else {
        rounded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_units_convert_speed_flow_temperature_pressure_and_distance() {
        let units = Units {
            speed: Speed::Mph,
            mass_flow: MassFlow::PoundsPerMinute,
            temperature: Temperature::Fahrenheit,
            pressure: Pressure::Psi,
            distance: Distance::Miles,
        };
        let close = |actual: Option<f64>, expected: f64| {
            assert!((actual.unwrap() - expected).abs() < 0.000_001)
        };
        close(
            units.conversion("kilometersPerHour").apply(100.0),
            62.1371192237,
        );
        close(
            units.conversion("gramsPerSecond").apply(453.59237 / 60.0),
            1.0,
        );
        close(units.conversion("celsius").apply(-40.0), -40.0);
        close(units.conversion("celsius").apply(100.0), 212.0);
        close(units.conversion("kilopascal").apply(100.0), 14.503773773);
        close(units.conversion("kilometers").apply(1.609344), 1.0);
        close(Units::default().conversion("mph").apply(1.0), 1.609344);
        close(Units::default().conversion("°F").apply(212.0), 100.0);
        close(
            Units {
                mass_flow: MassFlow::KilogramsPerHour,
                ..Default::default()
            }
            .conversion("g/s")
            .apply(10.0),
            36.0,
        );
        assert!(units.conversion("km/h").apply(f64::MAX).is_some());
        assert!(Units::default().conversion("mph").apply(f64::MAX).is_none());
    }

    #[test]
    fn display_conversion_preserves_original_evidence_and_structured_limits() {
        let reading = Reading {
            source: Some(0x7e8),
            request: "010D".into(),
            name: "Vehicle speed".into(),
            value: "100".into(),
            unit: "kilometersPerHour".into(),
            numeric: Some(100.0),
            raw: "410D64".into(),
            status: "ECU responded".into(),
            signal_failure: false,
        };
        let units = Units {
            speed: Speed::Mph,
            ..Default::default()
        };
        let display = units.reading(&reading);
        assert_eq!(display.value, "62.14");
        assert_eq!(display.unit, "mph");
        assert_eq!(display.raw, reading.raw);
        assert_eq!(reading.value, "100");
        let again = units.reading(&display);
        assert_eq!(again.value, "62.14");
        let structured = Reading {
            value: "100 [limits 90..110]".into(),
            ..reading.clone()
        };
        assert_eq!(units.reading(&structured).value, structured.value);
        assert_eq!(units.reading(&structured).unit, structured.unit);
        let missing = Reading {
            value: "Unavailable".into(),
            numeric: None,
            ..reading
        };
        assert_eq!(units.reading(&missing).value, "Unavailable");
        assert_eq!(units.reading(&missing).unit, "mph");
        assert_ne!(format_number(0.000_000_1), "0.00");
    }

    #[test]
    fn monitor_values_and_limits_keep_protocol_units() {
        let units = Units {
            pressure: Pressure::Psi,
            ..Default::default()
        };
        let monitor = Reading {
            source: Some(0x7e8),
            request: "0621".into(),
            name: "Pressure monitor".into(),
            value: "100".into(),
            unit: "kilopascal".into(),
            numeric: Some(100.0),
            raw: "4621".into(),
            status: "PASS".into(),
            signal_failure: false,
        };
        assert_eq!(units.reading(&monitor).value, "100");
        assert_eq!(units.reading(&monitor).unit, "kilopascal");
    }

    #[test]
    fn converted_temperature_graphs_keep_gaps_and_correct_ranges() {
        use super::super::model::GraphPoint;
        let points = [
            GraphPoint {
                at_ms: 0,
                value: Some(-40.0),
            },
            GraphPoint {
                at_ms: 1,
                value: None,
            },
            GraphPoint {
                at_ms: 2,
                value: Some(100.0),
            },
        ];
        let conversion = Units {
            temperature: Temperature::Fahrenheit,
            ..Default::default()
        }
        .conversion("celsius");
        let trace = super::super::graphs::Trace::new(
            points.iter().map(|point| GraphPoint {
                at_ms: point.at_ms,
                value: point.value.and_then(|value| conversion.apply(value)),
            }),
            0,
            2,
        );
        assert_eq!(trace.range, Some((-40.0, 212.0)));
        assert_eq!(trace.runs.len(), 2);
        assert!(trace.point_near(1, 0).is_none());
        assert_eq!(points[2].value, Some(100.0));
    }
}
