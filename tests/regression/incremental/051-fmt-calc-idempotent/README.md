This scenario generates its own fixture at runtime (see run.sh).
It verifies that `hard fmt` preserves `calc name(...)` function syntax and
is idempotent, and that the formatted file still builds.