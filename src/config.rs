use std::str::FromStr;

use device_query::Keycode;
use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
pub struct GeneralConfig {
    pub push_to_talk: bool,
    #[serde(deserialize_with = "deserialize_keycode")]
    pub ptt_key: Keycode
}

fn deserialize_keycode<'de, D>(deserializer: D) -> Result<Keycode, D::Error>
where
    D: serde::Deserializer<'de>
{
    let s = String::deserialize(deserializer)?;
    Keycode::from_str(&s).map_err(serde::de::Error::custom)
}
