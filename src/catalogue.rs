use reqwest::Url;

use crate::cli::parse_url;

pub const ROOTS_BASE_URL: &str = "http://127.0.0.1:4301";

#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub id: &'static str,
    pub label: &'static str,
    pub path: &'static str,
    pub expected_status: u16,
    pub description: &'static str,
}

impl Target {
    pub fn url(&self, base: &Url) -> Url {
        base.join(self.path)
            .expect("catalogue paths must be valid URL references")
    }
}

pub const ROOTS_TARGETS: &[Target] = &[
    Target {
        id: "roots-hello",
        label: "Roots greeting",
        path: "/api/hello",
        expected_status: 200,
        description: "Static greeting; no per-request database work.",
    },
    Target {
        id: "roots-health",
        label: "Roots database health",
        path: "/api/health",
        expected_status: 200,
        description: "PostgreSQL connection and SELECT 1; requires a healthy database.",
    },
    Target {
        id: "roots-openapi",
        label: "Roots OpenAPI document",
        path: "/api/openapi.json",
        expected_status: 200,
        description: "Served API schema; status check only, without schema validation.",
    },
    Target {
        id: "roots-docs",
        label: "Roots Swagger UI",
        path: "/api/docs/",
        expected_status: 200,
        description: "Documentation HTML; trailing slash avoids the redirect.",
    },
    Target {
        id: "roots-session-signed-out",
        label: "Roots session access check",
        path: "/api/auth/me",
        expected_status: 401,
        description: "Signed-out rejection only; does not measure authenticated session lookup.",
    },
    Target {
        id: "roots-customer-signed-out",
        label: "Roots customer profile access check",
        path: "/api/customers/me",
        expected_status: 401,
        description: "Signed-out rejection only; does not measure customer profile reads.",
    },
    Target {
        id: "roots-strains-signed-out",
        label: "Roots strain catalogue access check",
        path: "/api/strains?limit=1",
        expected_status: 401,
        description: "Signed-out rejection only; does not query the shared strain catalogue.",
    },
    Target {
        id: "roots-experiences-signed-out",
        label: "Roots experience history access check",
        path: "/api/experiences?limit=1",
        expected_status: 401,
        description: "Signed-out rejection only; does not read private journal history.",
    },
    Target {
        id: "roots-calendar-signed-out",
        label: "Roots experience calendar access check",
        path: "/api/experiences/calendar?month=2026-09",
        expected_status: 401,
        description: "Signed-out rejection only; fixed sample month, without reading calendar data.",
    },
];

pub fn parse_target(input: &str) -> Result<Target, String> {
    ROOTS_TARGETS
        .iter()
        .find(|target| target.id == input)
        .copied()
        .ok_or_else(|| "unknown target; run `rusta-pitest catalogue` to list target IDs".into())
}

pub fn parse_base_url(input: &str) -> Result<Url, String> {
    let url = parse_url(input)?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "expected an HTTP(S) origin without credentials, path, query or fragment".into(),
        );
    }
    Ok(url)
}

pub fn print() {
    println!("Roots target catalogue (source reviewed 2026-09-27)");
    println!("Default API: {ROOTS_BASE_URL}");
    println!("Listing sends no requests. All targets use GET without credentials.\n");
    for target in ROOTS_TARGETS {
        println!("{} — {}", target.id, target.label);
        println!(
            "  GET {} — expect HTTP {}",
            target.path, target.expected_status
        );
        println!("  {}", target.description);
        println!("  Run: rusta-pitest target {}\n", target.id);
    }
    println!("Override the API origin with --base-url http://127.0.0.1:PORT.");
    println!("Use --requests or --duration-secs and --concurrency for a benchmark.");
    println!("A passing 401 checks access rejection, not authenticated feature performance.");
}
