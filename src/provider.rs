#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CableRoute {
    pub render: Device,
    pub microphone: Device,
}

// Provider-owned endpoint names are paired; an arbitrary WASAPI output is not a microphone.
// VB-CABLE is independently installed donationware. No driver is bundled or installed by this app.
pub fn cable_routes(outputs: &[Device], inputs: &[Device]) -> Vec<CableRoute> {
    let mut routes = Vec::new();
    for render in outputs {
        if let Some(family) = endpoint_family(&render.name, "input") {
            let matches: Vec<_> = inputs
                .iter()
                .filter(|input| endpoint_family(&input.name, "output") == Some(family))
                .collect();
            if matches.len() == 1 {
                routes.push(CableRoute {
                    render: render.clone(),
                    microphone: matches[0].clone(),
                });
            }
        }
    }
    routes
}

fn endpoint_family(name: &str, direction: &str) -> Option<&'static str> {
    // Keep provider detection strict. Similar speaker names must never become an implicit monitor.
    let (prefix, suffix) = name.split_once(" (")?;
    if !suffix.to_ascii_lowercase().contains("vb-audio") {
        return None;
    }
    for family in ["CABLE", "CABLE-A", "CABLE-B", "CABLE-C", "CABLE-D"] {
        if prefix.eq_ignore_ascii_case(&format!("{family} {direction}")) {
            return Some(family);
        }
    }
    None
}

pub fn is_cable_capture(name: &str) -> bool {
    endpoint_family(name, "output").is_some()
}

pub const PROVIDER_URL: &str = "https://vb-audio.com/Cable/";
