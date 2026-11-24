# `jq -s --arg version X -f sbom-merge.jq exe.cdx.json apo.cdx.json` (release.yml): cargo-cyclonedx
# writes one CycloneDX file per binary, the release carries one for the whole package. The package
# is the root and depends on the binaries; a crate both binaries use is listed once.
"GamersEqualizer" as $root
| . as $boms
| $boms[0]
| .metadata.component = {type: "application", "bom-ref": $root, name: $root, version: $version}
| .components = (
    [$boms[] | .metadata.component, .components[]]
    | group_by(."bom-ref")
    # A crate that one binary links and the other only builds with is `required`.
    | map((map(select(.scope != "excluded")) | first) // first)
  )
# Cargo resolves the workspace as a whole, so a crate's entry is the same in both files.
| .dependencies = [{ref: $root, dependsOn: [$boms[].metadata.component."bom-ref"]}]
    + ([$boms[].dependencies[]] | unique_by(.ref))
# Self-check: every edge of the dependency graph ends at a listed component.
| (([.dependencies[] | .ref, .dependsOn[]?] | unique) - [$root, .components[]."bom-ref"]) as $lost
| if $lost == [] then . else error("dependencies without a component: \($lost)") end
