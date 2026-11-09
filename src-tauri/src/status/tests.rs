use super::*;

/// A full report captured from an X2D mid-print, serials and addresses redacted.
const X2D: &str = include_str!("fixtures/x2d_push_status.json");

fn status_after(reports: &[&str]) -> PrinterStatus {
    let mut status = PrinterStatus::default();
    for report in reports {
        status.apply(report.as_bytes()).unwrap();
    }
    status
}

#[test]
fn x2d_full_report() {
    let s = status_after(&[X2D]);

    assert_eq!((s.nozzle_temp, s.nozzle_target), (220.0, 220.0));
    assert_eq!(
        s.nozzles,
        vec![
            Nozzle {
                side: Side::Left,
                current: 220.0,
                target: 220.0,
                active: true
            },
            Nozzle {
                side: Side::Right,
                current: 39.0,
                target: 0.0,
                active: false
            },
        ]
    );
    assert_eq!(
        s.chamber,
        Some(Temperature {
            current: 31.0,
            target: 0.0
        })
    );
    assert_eq!((s.bed_temp, s.bed_target), (55.0, 55.0));
    assert_eq!((s.progress, s.layer_num, s.total_layer_num), (87, 159, 188));
    assert_eq!(s.gcode_state, "RUNNING");
    assert_eq!(s.tray_now, 3);
    assert_eq!(s.ams.len(), 1);
    assert_eq!(s.ams[0].trays.len(), 4);
    assert_eq!(
        s.camera,
        Camera::Rtsp {
            url: "rtsps://192.0.2.10:322/streaming/live/1".into()
        }
    );

    // Only the right holder carries a spool, and no extruder is fed from it.
    assert_eq!(s.external_spools.len(), 1);
    let spool = &s.external_spools[0];
    assert_eq!(
        (spool.slot, spool.side, spool.loadable, spool.loaded),
        (255, Some(Side::Right), false, false)
    );
    assert_eq!(
        (spool.tray.tray_type.as_str(), spool.tray.color.as_str()),
        ("PLA", "161616")
    );
}

#[test]
fn partial_report_changes_only_its_fields() {
    let s = status_after(&[
        X2D,
        r#"{"print":{"command":"push_status","mc_percent":88,"ams":{"tray_now":"2"}}}"#,
    ]);

    assert_eq!((s.progress, s.tray_now), (88, 2));
    assert_eq!(s.layer_num, 159);
    assert_eq!(s.nozzles.len(), 2);
    assert_eq!(s.ams.len(), 1);
    assert_eq!(s.external_spools.len(), 1);
    assert!(matches!(s.camera, Camera::Rtsp { .. }));
}

#[test]
fn malformed_field_leaves_the_rest_of_the_report() {
    let s = status_after(&[r#"{"print":{
        "mc_percent": "42",
        "layer_num": {"unexpected": true},
        "gcode_state": "PAUSE",
        "ams": {"tray_now": "5", "ams": [{"id": "1", "humidity": "3", "tray": [
            {"id": "1", "tray_type": "PETG", "tray_color": "FF0000FF", "tray_temp": "250"}
        ]}]}
    }}"#]);

    assert_eq!(s.progress, 42);
    assert_eq!(s.layer_num, 0);
    assert_eq!(s.gcode_state, "PAUSE");
    assert_eq!(s.tray_now, 5);
    assert_eq!(
        s.ams[0].trays[0],
        AmsTray {
            id: 1,
            tray_type: "PETG".into(),
            color: "FF0000".into(),
            name: String::new(),
            tray_temp: 250,
        }
    );
    assert_eq!(s.tray_temperature(5), Some(250));
}

#[test]
fn single_nozzle_printer_without_chamber_sensor() {
    // A1/P1 shape: a constant chamber_temper, no ipcam URL, the external spool in vt_tray.
    let s = status_after(&[r#"{"print":{
        "command": "push_status",
        "chamber_temper": 5,
        "device": {"extruder": {"state": 1, "info": [{"id": 0, "temp": 13762770}]}},
        "ams": {"tray_now": "254"},
        "vt_tray": {"id": "254", "tray_type": "PLA", "tray_color": "FFFFFFFF", "tray_temp": "220"}
    }}"#]);

    assert_eq!(s.chamber, None);
    assert!(s.nozzles.is_empty());
    assert_eq!(s.camera, Camera::Jpeg);
    let spool = &s.external_spools[0];
    assert_eq!(
        (spool.slot, spool.side, spool.loadable, spool.loaded),
        (EXTERNAL_SPOOL_SLOT, None, true, true)
    );
}

#[test]
fn external_holder_feeding_an_extruder_is_loaded() {
    // snow = ams_index << 8 | tray_index; 0xFF00 is the holder with vir_slot ID 255.
    let s = status_after(&[r#"{"print":{
        "device": {"extruder": {"state": 2, "info": [
            {"id": 0, "temp": 0, "snow": 65280},
            {"id": 1, "temp": 0, "snow": 65535}
        ]}},
        "vir_slot": [{"id": "254", "tray_type": "PLA"}, {"id": "255", "tray_type": "TPU"}]
    }}"#]);

    let spools: Vec<_> = s
        .external_spools
        .iter()
        .map(|sp| (sp.side, sp.loaded))
        .collect();
    assert_eq!(
        spools,
        vec![(Some(Side::Left), false), (Some(Side::Right), true)]
    );
}
