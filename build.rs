#![allow(nonstandard_style)]

// Generates `$OUT_DIR/webAssets.rs` from every file in `web/dist/`.
//
// Each asset becomes its own Rocket `#[get]` handler that embeds the file with
// `include_bytes!`, so the running binary needs no asset directory on disk.
// The generated file also exposes `fileRoutes()`, which returns every handler as
// a `Vec<rocket::Route>` ready for `.mount("/", ...)`.
//
// The output is included from `src/main.rs`:
//   mod webAssets { include!(concat!(env!("OUT_DIR"), "/webAssets.rs")); }

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

// URLs already owned by hand-written Rocket handlers. Embedding a frontend file
// at one of these would make Rocket panic at ignition with a route collision.
const reservedRoutes: &[&str] = &["/health"];

struct Asset {
	route: String,
	// Path relative to `web/dist`, always using forward slashes.
	relative: String,
	extension: String,
}

fn main() {
	// Cargo reruns a build script when a `rerun-if-changed` path changes. Cargo
	// scans directories recursively, so tracking `web/dist` covers every asset.
	// (Do not track `web` itself: `web/node_modules` can be enormous.)
	println!("cargo:rerun-if-changed=build.rs");
	println!("cargo:rerun-if-changed=web/dist");

	let manifestDir = PathBuf::from(
		env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
	);
	let distDir = manifestDir.join("web").join("dist");
	let outDir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by Cargo"));
	let outputPath = outDir.join("webAssets.rs");

	let mut assets = Vec::new();
	if distDir.is_dir() {
		collectAssets(&distDir, &distDir, &mut assets);
	} else {
		// Keep `cargo check` usable before the frontend has ever been built.
		println!(
			"cargo:warning=web/dist not found; serving no frontend assets (run the frontend build first)"
		);
	}

	// Directory iteration order is unspecified, so sort to keep the generated
	// file byte-for-byte stable across builds.
	assets.sort_by(|a, b| a.route.cmp(&b.route));
	checkAssetRoutes(&assets);

	fs::write(&outputPath, generateModule(&assets))
		.unwrap_or_else(|e| panic!("failed to write {}: {}", outputPath.display(), e));
}

fn collectAssets(root: &Path, dir: &Path, assets: &mut Vec<Asset>) {
	let entries = match fs::read_dir(dir) {
		Ok(entries) => entries,
		Err(e) => panic!("failed to read {}: {}", dir.display(), e),
	};

	for entry in entries {
		let entry = match entry {
			Ok(entry) => entry,
			Err(e) => panic!("failed to read an entry in {}: {}", dir.display(), e),
		};

		let path = entry.path();
		let fileName = entry.file_name().to_string_lossy().into_owned();

		// Hidden files (`.DS_Store`, editor temp files, ...) are not assets.
		if fileName.starts_with('.') {
			continue;
		}

		if path.is_dir() {
			collectAssets(root, &path, assets);
		} else if path.is_file() {
			let relative = path
				.strip_prefix(root)
				.expect("walked path is under web/dist")
				.to_string_lossy()
				.replace('\\', "/");

			// `index.html` is the page, so it lives at the root URL. Every other
			// file keeps its path (e.g. `js/app.js` -> `/js/app.js`).
			let route = if relative == "index.html" {
				"/".to_string()
			} else {
				format!("/{}", relative)
			};

			validateRoute(&route, &relative);

			// Unknown extensions are fine; the generated handler falls back to
			// `application/octet-stream` at request time.
			let extension = path
				.extension()
				.and_then(|extension| extension.to_str())
				.map(|extension| extension.to_ascii_lowercase())
				.filter(|extension| extension.chars().all(|c| c.is_ascii_alphanumeric()))
				.unwrap_or_default();

			assets.push(Asset {
				route,
				relative,
				extension,
			});
		}
	}
}

fn validateRoute(route: &str, relative: &str) {
	// Rocket route paths are static URIs. `<`, `>`, `?`, `#`, `%` and spaces
	// would change their meaning (or be rejected), so fail the build instead of
	// emitting a handler that cannot be mounted safely.
	let isAllowed = route
		.chars()
		.all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '~'));

	if !isAllowed {
		panic!(
			"web/dist/{} cannot be served: route {:?} contains characters that are not safe in a Rocket route path (allowed: ASCII letters, digits, '.', '_', '-', '~' and '/')",
			relative, route
		);
	}
}

fn checkAssetRoutes(assets: &[Asset]) {
	let mut seen = HashSet::new();

	for asset in assets {
		// Windows filesystems are case-insensitive, so `Logo.svg` and `logo.svg`
		// cannot both be served by distinct route strings predictably.
		let key = asset.route.to_ascii_lowercase();
		if !seen.insert(key) {
			panic!(
				"web/dist contains two files that would both be served at {:?} (paths are compared case-insensitively)",
				asset.route
			);
		}

		let conflictsWithApi = asset.route == "/api" || asset.route.starts_with("/api/");
		let conflictsWithReserved = reservedRoutes.contains(&asset.route.as_str());
		if conflictsWithApi || conflictsWithReserved {
			panic!(
				"web/dist file {:?} would be served at {:?}, which collides with a hand-written route",
				asset.relative, asset.route
			);
		}
	}
}

fn generateModule(assets: &[Asset]) -> String {
	let mut code = String::new();

	// Emacs modeline: mark the file as generated text so it is not hand-edited.
	code.push_str("// -*- mode: text -*-\n");
	code.push_str("// @generated by build.rs -- do not edit.\n\n");

	for (index, asset) in assets.iter().enumerate() {
		// `include_bytes!` resolves relative to the file that contains it, which
		// is `$OUT_DIR/webAssets.rs`. Anchor the path at CARGO_MANIFEST_DIR so it
		// still points into `web/dist` (and works on Windows).
		let sourceSuffix = format!("/web/dist/{}", asset.relative);

		code.push_str(&format!(
			"#[get(\"{route}\")]\nfn file{index}() -> (rocket::http::ContentType, &'static [u8]) {{\n\t(\n\t\trocket::http::ContentType::from_extension(\"{extension}\")\n\t\t\t.unwrap_or(rocket::http::ContentType::Binary),\n\t\tinclude_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"{source}\")),\n\t)\n}}\n\n",
			route = asset.route,
			index = index,
			extension = asset.extension,
			source = sourceSuffix,
		));
	}

	// Rocket's `routes!` macro evaluates to `Vec<Route>`; this is the aggregate
	// of every generated file-serving handler. An empty asset set yields an
	// empty vec, which `mount` accepts.
	code.push_str("pub fn fileRoutes() -> Vec<rocket::Route> {\n\trocket::routes![");
	for (index, _) in assets.iter().enumerate() {
		if index > 0 {
			code.push_str(", ");
		}
		code.push_str(&format!("file{}", index));
	}
	code.push_str("]\n}\n");

	code
}
