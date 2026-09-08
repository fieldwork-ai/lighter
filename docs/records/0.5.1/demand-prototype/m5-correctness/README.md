# M5 correctness at 57456cf

Both demand flags enabled, isolated test homes, qualified guest payload reused.
The user's daily 0.5.0 VM stayed running with its configuration and identity
unchanged. These are functional checks on a shared host, not M5 performance
measurements. All eleven functional gates passed; the speed gate was excluded
under the user's instruction. Workspace: 343 pass; signed hardware: 22 pass.

The first workspace attempt inherited a 256-file descriptor limit and failed
existing filesystem budget tests with EMFILE. The complete retained rerun uses
8192 descriptors in the test shell; the gates use 10240. No runtime workaround
was introduced. Original failure log remains in .logs/051/demand.

Compressed text preserves gate output with only checkout/home paths replaced.
Elapsed gate durations are operational records, not benchmark results. Gate
binaries were built from the clean detached 57456cf worktree; these are not
signed release-archive qualification results. The final release requires its
own frozen artifact checks.
