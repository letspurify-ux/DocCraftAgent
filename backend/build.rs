fn main() {
    // sqlx::migrate! embeds migrations in the binary. Make incremental builds
    // rebuild that binary whenever a migration is added or edited.
    println!("cargo:rerun-if-changed=migrations");
}
