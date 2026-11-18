use std::{collections::hash_map::RandomState, hash::BuildHasher, time::SystemTime};

use chrono::Timelike;

#[derive(Clone, Copy)]
pub(super) enum Style {
    TimeOfDay,
    Hello,
    Greetings,
}

impl Style {
    pub(super) fn choose() -> Self {
        match RandomState::new().hash_one(SystemTime::now()) % 4 {
            0 => Self::Hello,
            1 => Self::Greetings,
            _ => Self::TimeOfDay,
        }
    }

    pub(super) fn prefix(self) -> &'static str {
        match self {
            Self::Hello => "Hello",
            Self::Greetings => "Greetings",
            Self::TimeOfDay => match chrono::Local::now().hour() {
                5..=11 => "Good morning",
                12..=17 => "Good afternoon",
                _ => "Good evening",
            },
        }
    }
}
