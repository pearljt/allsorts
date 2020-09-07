pub mod arabic;
pub mod indic;
pub mod syriac;

use crate::tag;

use std::convert::From;

#[derive(std::cmp::PartialEq)]
pub enum ScriptType {
    Arabic,
    Default,
    Indic,
    Syriac,
}

impl From<u32> for ScriptType {
    fn from(script_tag: u32) -> Self {
        match script_tag {
            tag::ARAB => ScriptType::Arabic,
            tag::LATN => ScriptType::Default,
            tag::CYRL => ScriptType::Default,
            tag::GREK => ScriptType::Default,
            tag::DEVA => ScriptType::Indic,
            tag::BENG => ScriptType::Indic,
            tag::GURU => ScriptType::Indic,
            tag::GUJR => ScriptType::Indic,
            tag::ORYA => ScriptType::Indic,
            tag::TAML => ScriptType::Indic,
            tag::TELU => ScriptType::Indic,
            tag::KNDA => ScriptType::Indic,
            tag::MLYM => ScriptType::Indic,
            tag::SYRC => ScriptType::Syriac,
            _ => ScriptType::Default,
        }
    }
}
