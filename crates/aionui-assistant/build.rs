//! `include_dir!` cannot tell cargo which files it embeds, so a new or changed
//! built-in assistant (manifest, rule or avatar) would otherwise leave this
//! crate unbuilt. Rebuild whenever anything under the assistants folder changes.
fn main() {
    println!("cargo:rerun-if-changed=../aionui-app/assets/builtin-assistants");
}
