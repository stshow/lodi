# `[sources]` stanza goldens (LD-365, S1)

Each `lodi-*.sources` file here is the deb822 stanza `hostscope::sourceset::render` writes for one
declaration of `golden_cases()` in `tests/host_sources.rs`. They are rendered by the code and never
edited by hand. `s1_the_stanza_is_rendered_byte_exact_as_the_goldens` compares every byte.

To re-derive them after a deliberate change of the writer:

    LODI_RECORD_EXPECTED=1 cargo test --locked --test host_sources s1_the_stanza_is_rendered_byte_exact_as_the_goldens

and review the diff: a changed stanza is a changed file on every machine that applies it.
