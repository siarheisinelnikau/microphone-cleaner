use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let path = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("microphone.ico");
    fs::write(&path, icon()).expect("write application icon");
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(path.to_str().unwrap())
        .set_manifest(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity version="0.1.0.0" processorArchitecture="*" name="MicrophoneCleaner" type="win32"/>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges>
    <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
  </requestedPrivileges></security></trustInfo>
  <dependency><dependentAssembly><assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/></dependentAssembly></dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings>
    <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
    <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
  </windowsSettings></application>
</assembly>"#)
        .set("ProductName", "Microphone Cleaner")
        .set("FileDescription", "Microphone Cleaner — local audio cleanup")
        .set("LegalCopyright", "Copyright (c) 2026 Microphone Cleaner contributors");
    resource.compile().expect("compile Windows resources");
}

fn icon() -> Vec<u8> {
    let sizes = [16u32, 20, 24, 32, 48, 64, 128, 256];
    let mut bytes = vec![0, 0, 1, 0, sizes.len() as u8, 0];
    let mut images = Vec::new();
    let mut offset = 6 + 16 * sizes.len() as u32;
    for size in sizes {
        let stride = size.div_ceil(32) * 4;
        let length = 40 + size * size * 4 + stride * size;
        bytes.extend_from_slice(&[(size % 256) as u8, (size % 256) as u8, 0, 0]);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&32u16.to_le_bytes());
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
        offset += length;
        let mut dib = Vec::with_capacity(length as usize);
        for value in [40, size, size * 2] {
            dib.extend_from_slice(&value.to_le_bytes());
        }
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&32u16.to_le_bytes());
        for value in [0u32, size * size * 4, 0, 0, 0, 0] {
            dib.extend_from_slice(&value.to_le_bytes());
        }
        for y in (0..size).rev() {
            for x in 0..size {
                let px = (x as f32 + 0.5) / size as f32;
                let py = (y as f32 + 0.5) / size as f32;
                let capsule = ((px - 0.5).abs() < 0.13 && (0.20..0.55).contains(&py))
                    || ((px - 0.5).powi(2) + (py - 0.20).powi(2) < 0.13f32.powi(2))
                    || ((px - 0.5).powi(2) + (py - 0.55).powi(2) < 0.13f32.powi(2));
                let arc = (0.29..0.71).contains(&px)
                    && (0.49..0.76).contains(&py)
                    && ((px - 0.5).powi(2) + (py - 0.51).powi(2)).sqrt() > 0.18
                    && ((px - 0.5).powi(2) + (py - 0.51).powi(2)).sqrt() < 0.25;
                let stand = ((px - 0.5).abs() < 0.035 && (0.71..0.89).contains(&py))
                    || ((px - 0.5).abs() < 0.16 && (0.85..0.91).contains(&py));
                let circle = (px - 0.5).powi(2) + (py - 0.5).powi(2) < 0.49f32.powi(2);
                dib.extend_from_slice(if capsule || arc || stand {
                    &[247, 255, 250, 255]
                } else if circle {
                    &[111, 104, 20, 255]
                } else {
                    &[0, 0, 0, 0]
                });
            }
        }
        dib.resize(length as usize, 0);
        images.extend_from_slice(&dib);
    }
    bytes.extend(images);
    bytes
}
