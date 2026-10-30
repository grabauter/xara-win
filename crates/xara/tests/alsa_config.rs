#![cfg(all(feature = "alsa", unix))]

use xara::audio::alsa::{Route, check_device, resolve_playback};

const CONF: &str = r#"
pcm.null { type null }
pcm.hw {
    @args [ CARD DEV ]
    @args.CARD { type string default 0 }
    @args.DEV { type integer default 0 }
    type hw
    card $CARD
    device $DEV
}
pcm.plughw {
    @args [ CARD DEV ]
    @args.CARD { type string default 0 }
    @args.DEV { type integer default 0 }
    type plug
    slave.pcm { type hw card $CARD device $DEV }
}
pcm.plug {
    @args [ SLAVE ]
    @args.SLAVE { type string }
    type plug
    slave.pcm $SLAVE
}
pcm.pulse { type pulse }
pcm.pipewire { type pipewire }
pcm.!default { type plug slave.pcm "pipewire" }
pcm_slave.radio_slave { pcm "hw:XaraNoSuchCard,0" rate 48000 }
pcm.radio_named_slave { type plug slave radio_slave }
pcm.radio_dmix { type dmix ipc_key 7193 slave { pcm "hw:XaraNoSuchCard,0" } }
pcm.radio_asym { type asym playback.pcm radio_dmix capture.pcm "pipewire" }
pcm.radio_file { type file slave.pcm null file "/dev/null" }
pcm.radio_alias "plughw:XaraNoSuchCard"
pcm.via_pipewire { type plug slave.pcm "pipewire" }
pcm.via_default { type route slave.pcm "default" ttable.0.0 1 }
pcm.inline_pulse { type softvol slave.pcm { type pulse } control.name xara }
pcm.asym_pulse { type asym playback.pcm "pulse" }
pcm.speex_hw { type speex slave.pcm "hw:XaraNoSuchCard,0" }
pcm.file {
    @args [ FILE FORMAT ]
    @args.FILE { type string }
    @args.FORMAT { type string default raw }
    type file
    slave.pcm null
    file $FILE
    format $FORMAT
}
pcm.tee {
    @args [ SLAVE FILE FORMAT ]
    @args.SLAVE { type string }
    @args.FILE { type string }
    @args.FORMAT { type string default raw }
    type file
    slave.pcm $SLAVE
    file $FILE
    format $FORMAT
}
pcm.cap_pipe { type file slave.pcm null file "|true" }
pcm.cap_infile { type file slave.pcm null file "/dev/null" infile "| true" }
pcm.cap_octal { type file slave.pcm null file "\174true" }
pcm.via_pipe { type plug slave.pcm "cap_pipe" }
pcm_slave.pipe_slave { pcm "cap_pipe" }
pcm.via_pipe_slave { type plug slave pipe_slave }
pcm.asym_pipe { type asym playback.pcm "null" capture.pcm "via_pipe" }
pcm.alias_pipe "via_pipe"
pcm.inline_pipe { type plug slave.pcm { type file slave.pcm null file "|true" } }
"#;

#[test]
fn playback_names_resolve_through_the_alsa_configuration() {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("xara-alsa-test.conf");
    std::fs::write(&path, CONF).unwrap();
    unsafe { std::env::set_var("ALSA_CONFIG_PATH", &path) };

    assert_eq!(resolve_playback("null", false).unwrap(), Route::Null);
    for n in ["hw:XaraNoSuchCard,0", "radio_file", "plughw:CARD=XaraNoSuchCard,DEV=0", "default"] {
        assert!(resolve_playback(n, false).is_err(), "{n}");
    }
    for n in [
        "hw:XaraNoSuchCard,0",
        "hw:9,0",
        "plughw:CARD=XaraNoSuchCard,DEV=0",
        "plug:\"hw:9,0\"",
        "radio_named_slave",
        "radio_dmix",
        "radio_asym",
        "radio_alias",
    ] {
        assert_eq!(resolve_playback(n, true).map_err(|e| e.to_string()), Ok(Route::Card), "{n}");
    }
    assert_eq!(resolve_playback("radio_file", true).unwrap(), Route::Null);
    for (n, why) in [
        ("default", "default device"),
        ("pulse", "default device"),
        ("pipewire", "default device"),
        ("plug:pipewire", "goes through \"pipewire\""),
        ("via_pipewire", "goes through \"pipewire\""),
        ("via_default", "goes through \"default\""),
        ("inline_pulse", "\"pulse\" plugin"),
        ("asym_pulse", "goes through \"pulse\""),
        ("speex_hw", "\"speex\" plugin"),
        ("hw", "default card"),
        ("xara_nonexistent", "no ALSA pcm named"),
    ] {
        let e = resolve_playback(n, true).unwrap_err().to_string();
        assert!(e.contains(why), "{n}: {e}");
    }

    for n in [
        "cap_pipe",
        "cap_infile",
        "cap_octal",
        "via_pipe",
        "via_pipe_slave",
        "asym_pipe",
        "alias_pipe",
        "inline_pipe",
        "file:'|true'",
        "file:'\\174true'",
        "file:FILE='|true',FORMAT=raw",
        "tee:null,'|true'",
        "plug:cap_pipe",
        "plug:'tee:null,\"|true\"'",
    ] {
        let e = check_device(n, "capture").unwrap_err().to_string();
        assert!(e.contains("runs a command"), "{n}: {e}");
        assert!(resolve_playback(n, true).is_err(), "{n}");
    }
    for n in ["null", "radio_file", "file:/dev/null", "plughw:CARD=XaraNoSuchCard,DEV=0", "radio_asym", "pulse", "nope"]
    {
        assert!(check_device(n, "capture").is_ok(), "{n}");
    }
}
