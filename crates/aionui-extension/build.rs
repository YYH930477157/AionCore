//! `include_dir!` cannot tell cargo which files it embeds, so a new or changed
//! built-in skill would otherwise leave this crate (and its embedded corpus)
//! unbuilt. Rebuild whenever anything under the skills folder changes.
fn main() {
    println!("cargo:rerun-if-changed=../aionui-app/assets/builtin-skills");
}
