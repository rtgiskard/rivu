use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TagValues {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub release_date: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    #[serde(default)]
    pub replaygain_track_gain: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TagValuesRef<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    pub release_date: Option<&'a str>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub replaygain_track_gain: Option<f64>,
}

impl TagValues {
    pub fn as_view(&self) -> TagValuesRef<'_> {
        TagValuesRef {
            title: &self.title,
            artist: &self.artist,
            album: &self.album,
            release_date: self.release_date.as_deref(),
            track_number: self.track_number,
            disc_number: self.disc_number,
            replaygain_track_gain: self.replaygain_track_gain,
        }
    }
}

impl TagValuesRef<'_> {
    pub fn into_owned(self) -> TagValues {
        TagValues {
            title: self.title.to_owned(),
            artist: self.artist.to_owned(),
            album: self.album.to_owned(),
            release_date: self.release_date.map(str::to_owned),
            track_number: self.track_number,
            disc_number: self.disc_number,
            replaygain_track_gain: self.replaygain_track_gain,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagEncoding {
    #[default]
    Declared,
    Utf8,
    Gb18030,
    Big5,
    ShiftJis,
    EucKr,
    Windows1252,
    Windows1251,
}
