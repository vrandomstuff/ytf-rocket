#![allow(nonstandard_style)]

// Generates `$OUT_DIR/webAssets.rs` from every file in `web/dist/`.
//
// Each asset becomes its own Rocket `#[get]` handler that embeds the file with
// `include_bytes!`, so the running binary needs no asset directory on disk.
// The generated file also exposes `fileRoutes()`, which returns every handler as
// a `Vec<rocket::Route>` ready for `.mount("/", ...)`.
//
// Before scanning, the frontend dependency licenses are regenerated into
// `web/dist/` with `pnpm licenses ls --json`. That page and the copied license
// texts are then embedded like any other frontend asset.
//
// The output is included from `src/main.rs`:
//   mod webAssets { include!(concat!(env!("OUT_DIR"), "/webAssets.rs")); }

use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;
use time::OffsetDateTime;

// URLs already owned by hand-written Rocket handlers. Embedding a frontend file
// at one of these would make Rocket panic at ignition with a route collision.
const reservedRoutes: &[&str] = &["/health"];

// --- License page generation -------------------------------------------------
//
// Ported from safe-yt-viewer's generate_licenses.py. The page is generated into
// `web/dist` (not a separate `public` directory) so `build.rs` embeds it on the
// same pass that scans the rest of the frontend.

const SPDX_URL: &str = "https://spdx.org/licenses/{}.html";

const LICENSE_FILENAMES: &[&str] = &[
	"license",
	"license.txt",
	"license.md",
	"licence",
	"licence.txt",
	"licence.md",
	"copying",
	"copying.txt",
];

const MULTIPLE_VERSIONS_NOTE: &str =
	"NOTE: this package resolved to multiple versions with differing license texts.\n\
	 Each distinct text is reproduced below.\n\n";

// Both maps are intentionally empty. Keep the lookup/insertion code below so an
// override or extra entry can be added without changing the generator itself.
// Shape examples:
//   LICENSE_URL_OVERRIDES: &[("package-name", "https://example.com/LICENSE")]
//   EXTRA_PACKAGES: &[("Apache-2.0", &[ExtraPackage { ... }])]
const LICENSE_URL_OVERRIDES: &[(&str, &str)] = &[];
const EXTRA_PACKAGES: &[(&str, &[ExtraPackage])] = &[];

// A package entry from `pnpm licenses ls --json`. Most fields are optional in
// the JSON, and `licenseUrl` is the fallback used when no license file exists in
// the installed package.
#[derive(Deserialize)]
struct Package {
	name: Option<String>,
	#[serde(default)]
	versions: Vec<String>,
	#[serde(default)]
	paths: Vec<String>,
	homepage: Option<String>,
	description: Option<String>,
	#[serde(rename = "licenseUrl")]
	licenseUrl: Option<String>,
}

// Used only by EXTRA_PACKAGES for packages that are not installed by pnpm.
struct ExtraPackage {
	name: &'static str,
	versions: &'static [&'static str],
	homepage: Option<&'static str>,
	licenseUrl: Option<&'static str>,
}

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
	// Regenerate the license page when the frontend manifest/lockfile changes,
	// not only when web/dist happens to be rebuilt.
	println!("cargo:rerun-if-changed=web/package.json");
	println!("cargo:rerun-if-changed=web/pnpm-lock.yaml");

	let manifestDir = PathBuf::from(
		env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
	);
	let webDir = manifestDir.join("web");
	let distDir = webDir.join("dist");
	let outDir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by Cargo"));
	let outputPath = outDir.join("webAssets.rs");

	let mut assets = Vec::new();
	if distDir.is_dir() {
		// Best effort: a missing pnpm or a failed pnpm invocation must not make
		// `cargo check` unusable. Keep any previously generated license files.
		if let Err(e) = generateLicenses(&webDir, &distDir) {
			// Cargo warnings are one line each, so flatten any multi-line stderr
			// captured from pnpm.
			println!(
				"cargo:warning=license generation failed; web/dist license output may be stale or incomplete. Do not publish this server without reviewing the licenses: {}",
				e.replace('\n', " ").replace('\r', " ")
			);
		}
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

fn generateLicenses(webDir: &Path, distDir: &Path) -> Result<(), String> {
	let mut data = fetchLicenses(webDir)?;

	// EXTRA_PACKAGES is empty by default, but keep the insertion path so a
	// package that is not installed by pnpm can still be listed.
	for &(licenseName, packages) in EXTRA_PACKAGES {
		data.entry(licenseName.to_string())
			.or_default()
			.extend(packages.iter().map(extraToPackage));
	}

	let licenseTexts = collectLicenseTexts(&data, distDir)?;
	let output = distDir.join("licenses.html");
	let content = render(&data, &licenseTexts);
	writeIfChangedHtml(&output, &content)
}

fn fetchLicenses(webDir: &Path) -> Result<BTreeMap<String, Vec<Package>>, String> {
	let output = runPnpmLicenses(webDir)?;
	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr);
		return Err(format!(
			"pnpm licenses ls exited with {}: {}",
			output.status,
			stderr.trim()
		));
	}

	let stdout = String::from_utf8(output.stdout)
		.map_err(|e| format!("pnpm licenses output was not UTF-8: {}", e))?;
	serde_json::from_str(&stdout)
		.map_err(|e| format!("could not parse pnpm licenses output: {}", e))
}

fn runPnpmLicenses(webDir: &Path) -> Result<std::process::Output, String> {
	// On Windows pnpm is normally a .cmd shim. `Command::new("pnpm")` does not
	// always resolve those, so try the explicit shim name as well.
	let candidates: &[&str] = if cfg!(windows) {
		&["pnpm", "pnpm.cmd"]
	} else {
		&["pnpm"]
	};

	for program in candidates {
		match Command::new(program)
			.args(["licenses", "ls", "--json"])
			.current_dir(webDir)
			.output()
		{
			Ok(output) => return Ok(output),
			Err(e) if e.kind() == ErrorKind::NotFound => continue,
			Err(e) => return Err(format!("failed to run {}: {}", program, e)),
		}
	}

	// Last resort on Windows: cmd applies PATHEXT and finds pnpm.cmd even when
	// the direct spawns above did not.
	#[cfg(windows)]
	{
		match Command::new("cmd")
			.args(["/C", "pnpm", "licenses", "ls", "--json"])
			.current_dir(webDir)
			.output()
		{
			Ok(output) => Ok(output),
			Err(e) => Err(format!("failed to run cmd /C pnpm: {}", e)),
		}
	}

	#[cfg(not(windows))]
	{
		Err("pnpm not found on PATH".to_string())
	}
}

fn extraToPackage(extra: &ExtraPackage) -> Package {
	Package {
		name: Some(extra.name.to_string()),
		versions: extra.versions.iter().map(|version| (*version).to_string()).collect(),
		paths: Vec::new(),
		homepage: extra.homepage.map(str::to_string),
		description: None,
		licenseUrl: extra.licenseUrl.map(str::to_string),
	}
}

fn collectLicenseTexts(
	data: &BTreeMap<String, Vec<Package>>,
	distDir: &Path,
) -> Result<HashMap<String, String>, String> {
	let licenseDir = distDir.join("licenses");
	fs::create_dir_all(&licenseDir)
		.map_err(|e| format!("failed to create {}: {}", licenseDir.display(), e))?;

	let agent = httpAgent();
	let mut urls = HashMap::new();
	let mut expectedFiles = HashSet::new();

	for packages in data.values() {
		for pkg in packages {
			let Some(name) = pkg.name.as_deref() else {
				continue;
			};
			if urls.contains_key(name) {
				continue;
			}

			let filename = format!("LICENSE.{}.txt", slugify(name));
			let destination = licenseDir.join(&filename);

			if let Some(overrideUrl) = licenseUrlOverride(name) {
				// Match the Python script: an override is authoritative. If it
				// fails to download, there is no local-text fallback.
				match downloadLicense(&agent, overrideUrl) {
					Ok(bytes) => {
						expectedFiles.insert(filename.clone());
						writeIfChanged(&destination, &bytes)?;
						urls.insert(name.to_string(), format!("/licenses/{}", filename));
					}
					Err(e) => warnDownloadFailure(name, overrideUrl, &e),
				}
				continue;
			}

			let texts = distinctLicenseTexts(pkg);
			if !texts.is_empty() {
				expectedFiles.insert(filename.clone());
				writeIfChanged(&destination, &joinLicenseTexts(&texts))?;
				urls.insert(name.to_string(), format!("/licenses/{}", filename));
			} else if let Some(remoteUrl) = pkg.licenseUrl.as_deref() {
				match downloadLicense(&agent, remoteUrl) {
					Ok(bytes) => {
						expectedFiles.insert(filename.clone());
						writeIfChanged(&destination, &bytes)?;
						urls.insert(name.to_string(), format!("/licenses/{}", filename));
					}
					Err(e) => warnDownloadFailure(name, remoteUrl, &e),
				}
			}
		}
	}

	pruneStaleLicenseFiles(&licenseDir, &expectedFiles)?;
	Ok(urls)
}

fn licenseUrlOverride(name: &str) -> Option<&'static str> {
	LICENSE_URL_OVERRIDES
		.iter()
		.find(|(package, _)| *package == name)
		.map(|(_, url)| *url)
}

fn httpAgent() -> ureq::Agent {
	let config = ureq::Agent::config_builder()
		.timeout_global(Some(Duration::from_secs(30)))
		.build();
	ureq::Agent::new_with_config(config)
}

fn warnDownloadFailure(name: &str, url: &str, error: &str) {
	println!(
		"cargo:warning=license download failed for {name} ({}); its license text will be omitted. Do not publish this server without reviewing that license: {}",
		url,
		error.replace('\n', " ").replace('\r', " ")
	);
}

fn downloadLicense(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, String> {
	let mut response = agent.get(url).call().map_err(|e| e.to_string())?;
	if !response.status().is_success() {
		return Err(format!("HTTP {}", response.status()));
	}
	response.body_mut().read_to_vec().map_err(|e| e.to_string())
}

fn distinctLicenseTexts(pkg: &Package) -> Vec<Vec<u8>> {
	let mut texts = Vec::new();
	let mut seen = HashSet::new();

	for path in &pkg.paths {
		let Some(source) = findLicenseFile(Path::new(path)) else {
			continue;
		};
		let Ok(data) = fs::read(&source) else {
			continue;
		};
		if !seen.contains(&data) {
			seen.insert(data.clone());
			texts.push(data);
		}
	}

	texts
}

fn findLicenseFile(pkgDir: &Path) -> Option<PathBuf> {
	let entries = fs::read_dir(pkgDir).ok()?;
	let mut byLower = HashMap::new();

	for entry in entries.flatten() {
		let path = entry.path();
		if path.is_file() {
			let lower = entry.file_name().to_string_lossy().to_ascii_lowercase();
			byLower.insert(lower, path);
		}
	}

	for candidate in LICENSE_FILENAMES {
		if let Some(path) = byLower.get(*candidate) {
			return Some(path.clone());
		}
	}

	let mut names: Vec<String> = byLower.keys().cloned().collect();
	names.sort();
	for lower in names {
		if lower.starts_with("license") || lower.starts_with("licence") {
			return byLower.remove(&lower);
		}
	}

	None
}

fn joinLicenseTexts(texts: &[Vec<u8>]) -> Vec<u8> {
	if texts.len() == 1 {
		return texts[0].clone();
	}

	let mut joined = Vec::new();
	joined.extend_from_slice(MULTIPLE_VERSIONS_NOTE.as_bytes());
	for (index, text) in texts.iter().enumerate() {
		if index > 0 {
			joined.extend_from_slice(&textDivider());
		}
		joined.extend_from_slice(rstripAscii(text));
		joined.push(b'\n');
	}
	joined
}

fn textDivider() -> Vec<u8> {
	let mut divider = vec![b'\n'];
	divider.extend(std::iter::repeat(b'=').take(78));
	divider.extend_from_slice(b"\n\n");
	divider
}

fn rstripAscii(bytes: &[u8]) -> &[u8] {
	let mut end = bytes.len();
	while end > 0 && bytes[end - 1].is_ascii_whitespace() {
		end -= 1;
	}
	&bytes[..end]
}

fn writeIfChanged(path: &Path, bytes: &[u8]) -> Result<(), String> {
	match fs::read(path) {
		Ok(existing) if existing == bytes => return Ok(()),
		Ok(_) => {}
		Err(e) if e.kind() == ErrorKind::NotFound => {}
		Err(e) => return Err(format!("failed to read {}: {}", path.display(), e)),
	}

	fs::write(path, bytes).map_err(|e| format!("failed to write {}: {}", path.display(), e))
}

fn writeIfChangedHtml(path: &Path, content: &str) -> Result<(), String> {
	if let Ok(existing) = fs::read_to_string(path) {
		// The timestamp changes every run; compare without it so an unchanged
		// page is not rewritten (and web/dist is not needlessly touched).
		if stripTimestamp(&existing) == stripTimestamp(content) {
			return Ok(());
		}
	}

	fs::write(path, content.as_bytes())
		.map_err(|e| format!("failed to write {}: {}", path.display(), e))
}

fn stripTimestamp(text: &str) -> String {
	text.lines()
		.filter(|line| !(line.contains("Generated ") && line.trim_start().starts_with("<p>")))
		.collect::<Vec<_>>()
		.join("\n")
}

fn pruneStaleLicenseFiles(licenseDir: &Path, expected: &HashSet<String>) -> Result<(), String> {
	let entries = fs::read_dir(licenseDir)
		.map_err(|e| format!("failed to read {}: {}", licenseDir.display(), e))?;

	for entry in entries {
		let entry = entry.map_err(|e| {
			format!(
				"failed to read an entry in {}: {}",
				licenseDir.display(),
				e
			)
		})?;
		let path = entry.path();
		let name = entry.file_name().to_string_lossy().into_owned();

		// Only remove files this generator owns. The directory is dedicated to
		// generated license texts, but leaving unrelated files alone is safer.
		if path.is_file()
			&& name.starts_with("LICENSE.")
			&& name.ends_with(".txt")
			&& !expected.contains(&name)
		{
			fs::remove_file(&path)
				.map_err(|e| format!("failed to remove {}: {}", path.display(), e))?;
		}
	}

	Ok(())
}

fn render(data: &BTreeMap<String, Vec<Package>>, licenseTexts: &HashMap<String, String>) -> String {
	let mut packageCount = 0usize;
	let mut sections = String::new();

	let mut licenseNames: Vec<(&String, &Vec<Package>)> = data.iter().collect();
	licenseNames.sort_by_key(|(name, _)| name.to_lowercase());

	for (licenseName, packages) in licenseNames {
		packageCount += packages.len();

		let idLinks = spdxIds(licenseName)
			.into_iter()
			.map(|sid| {
				let escapedSid = htmlEscape(&sid);
				format!(
					"<a href=\"{}\" target=\"_blank\" rel=\"noopener\">{}</a>",
					SPDX_URL.replace("{}", &escapedSid),
					escapedSid
				)
			})
			.collect::<Vec<_>>()
			.join(", ");

		let mut sortedPackages: Vec<&Package> = packages.iter().collect();
		sortedPackages.sort_by_key(|pkg| {
			pkg.name
				.as_deref()
				.unwrap_or("unknown")
				.to_lowercase()
		});

		let mut items = String::new();
		for pkg in sortedPackages {
			let rawName = pkg.name.as_deref().unwrap_or("unknown");
			let name = htmlEscape(rawName);

			let version = pkg.versions.join(", ");
			let versionHtml = if version.is_empty() {
				String::new()
			} else {
				format!("<span class=\"version\">{}</span>", htmlEscape(&version))
			};

			let nameHtml = if let Some(homepage) = pkg.homepage.as_deref() {
				format!(
					"<a href=\"{}\" target=\"_blank\" rel=\"noopener\">{}</a>",
					htmlEscape(homepage),
					name
				)
			} else {
				name
			};

			let textHtml = if let Some(textUrl) = licenseTexts.get(rawName) {
				format!(
					"<a class=\"text-link\" href=\"{}\">license text</a>",
					htmlEscape(textUrl)
				)
			} else {
				String::new()
			};

			let descriptionHtml = if let Some(description) = pkg.description.as_deref() {
				format!("<small>{}</small>", htmlEscape(description))
			} else {
				String::new()
			};

			items.push_str(&format!(
				"<li>{}{}{}{}</li>",
				nameHtml, versionHtml, textHtml, descriptionHtml
			));
		}

		sections.push_str(&format!(
			"<section><h2>{}</h2><ul>{}</ul></section>",
			idLinks, items
		));
	}

	let generated = formatTimestamp();
	format!(
		r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Licenses</title>
<style>
	:root {{ color-scheme: dark; }}
	body {{
		font-family: system-ui, sans-serif;
		max-width: 50rem;
		margin: 2rem auto;
		padding: 0 1rem;
		color: #c0caf5;
		background: #24283b;
	}}
	h1 {{
		color: #7aa2f7;
		border-bottom: 2px solid #2f3549;
		padding-bottom: .5rem;
	}}
	h2 {{ margin-top: 2rem; font-size: 1.15rem; }}
	h2 a {{ color: #bb9af7; }}
	p {{ color: #7982a9; }}
	ul {{ list-style: none; padding-left: 0; }}
	li {{
		padding: .4rem 0;
		border-bottom: 1px solid #2f3549;
		line-height: 1.5;
	}}
	small {{ display: block; color: #7982a9; }}
	.version {{
		margin-left: .5rem;
		color: #ff9e64;
		font-family: ui-monospace, monospace;
		font-size: .85em;
	}}
	.text-link {{
		margin-left: .5rem;
		font-size: .85em;
		color: #9ece6a;
	}}
	a {{ color: #7dcfff; text-decoration-color: #565f89; }}
	a:hover {{ color: #c0caf5; }}
	a:visited {{ color: #7dcfff; }}
	::selection {{ background: #7aa2f7; color: #1f2335; }}
</style>
</head>
<body>
<h1>Licenses</h1>
<p>{packageCount} packages across {licenseCount} license(s). Generated {generated}.</p>
{sections}
</body>
</html>
"#,
		packageCount = packageCount,
		licenseCount = data.len(),
		generated = generated,
		sections = sections,
	)
}

fn spdxIds(licenseName: &str) -> Vec<String> {
	let mut ids = Vec::new();
	let mut current = String::new();

	for token in licenseName.trim().split_whitespace() {
		if token == "AND" || token == "OR" {
			if !current.is_empty() {
				ids.push(current);
				current = String::new();
			}
		} else {
			if !current.is_empty() {
				current.push(' ');
			}
			current.push_str(token);
		}
	}

	if !current.is_empty() {
		ids.push(current);
	}

	ids
}

fn slugify(name: &str) -> String {
	let mut slug = String::new();
	let mut pendingDash = false;

	for character in name.to_lowercase().chars() {
		if character.is_ascii_alphanumeric() || character == '.' {
			if pendingDash && !slug.is_empty() {
				slug.push('-');
			}
			pendingDash = false;
			slug.push(character);
		} else if !slug.is_empty() {
			pendingDash = true;
		}
	}

	slug
}

fn htmlEscape(value: &str) -> String {
	let mut escaped = String::with_capacity(value.len());

	for character in value.chars() {
		match character {
			'&' => escaped.push_str("&amp;"),
			'<' => escaped.push_str("&lt;"),
			'>' => escaped.push_str("&gt;"),
			'"' => escaped.push_str("&quot;"),
			'\'' => escaped.push_str("&#x27;"),
			_ => escaped.push(character),
		}
	}

	escaped
}

fn formatTimestamp() -> String {
	let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
	format!(
		"{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
		now.year(),
		now.month() as u8,
		now.day(),
		now.hour(),
		now.minute(),
		now.second()
	)
}
