use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
use lofty::tag::{Accessor, TagExt};
use std::{
    fs::{self, File},
    io::BufReader,
};

fn atom(ident: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut bytes = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(ident);
    bytes.extend_from_slice(payload);
    bytes
}

#[test]
fn explicit_patch_keeps_backup_and_unrelated_native_items() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("edit.m4a");
    let mut tag = Ilst::new();
    tag.set_title("old title".into());
    tag.set_artist("untouched artist".into());
    let custom = AtomIdent::Freeform {
        mean: "org.rivu.test".into(),
        name: "PRESERVE".into(),
    };
    tag.insert(Atom::new(
        custom.clone(),
        AtomData::UTF8("freeform value".into()),
    ));
    let mut ilst = Vec::new();
    tag.dump_to(&mut ilst, WriteOptions::default()).unwrap();
    let mut meta = vec![0; 4];
    meta.extend_from_slice(&ilst);
    let audio = b"encoded audio payload sentinel";
    let original = [
        atom(b"ftyp", b"M4A \0\0\0\0M4A "),
        atom(b"mdat", audio),
        atom(b"moov", &atom(b"udta", &atom(b"meta", &meta))),
    ]
    .concat();
    fs::write(&path, &original).unwrap();
    let report = rivu_metadata::edit_file(
        &path,
        &rivu_metadata::TagPatch {
            title: Some("edited title".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(report.changed);
    assert_eq!(fs::read(report.backup_path.unwrap()).unwrap(), original);
    let parsed = Mp4File::read_from(
        &mut BufReader::new(File::open(&path).unwrap()),
        ParseOptions::new().read_properties(false),
    )
    .unwrap();
    let edited = parsed.ilst().unwrap();
    assert_eq!(edited.title().as_deref(), Some("edited title"));
    assert_eq!(edited.artist(), tag.artist());
    assert_eq!(edited.get(&custom), tag.get(&custom));
    assert!(
        fs::read(path)
            .unwrap()
            .windows(audio.len())
            .any(|window| window == audio)
    );
}
