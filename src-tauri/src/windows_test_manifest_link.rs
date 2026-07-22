const LINKER_DIRECTIVES: &str = concat!(
    " /DEFAULTLIB:\"",
    env!("NEXQ_WINDOWS_TEST_RESOURCE_LIBRARY"),
    "\" /INCLUDE:NEXQ_WINDOWS_TEST_MANIFEST_RESOURCE",
);

const fn directive_bytes<const N: usize>(value: &str) -> [u8; N] {
    let value = value.as_bytes();
    let mut output = [0; N];
    let mut index = 0;

    while index < N {
        output[index] = value[index];
        index += 1;
    }

    output
}

// MSVC accepts `/DEFAULTLIB` in the COFF `.drectve` section. Keeping this module
// behind `cfg(test)` prevents the resource from affecting normal library or
// application builds.
#[used]
#[unsafe(link_section = ".drectve")]
static WINDOWS_TEST_MANIFEST_LINK_ARGS: [u8; LINKER_DIRECTIVES.len()] =
    directive_bytes::<{ LINKER_DIRECTIVES.len() }>(LINKER_DIRECTIVES);
