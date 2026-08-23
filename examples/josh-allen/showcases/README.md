# JOSH/ALLEN workflow examples

These programs use bounded fixtures. ALLEN performs the deterministic work,
then JOSH pauses for typed judgment or approval. Every program is a dry run.

| Scenario | Callback pattern | Prompt |
| --- | --- | --- |
| Repository migration | Agent exception decision | `Execute examples/josh-allen/showcases/guarded-repository-migration.allen.` |
| Test-failure reduction | Agent tie-breaker | `Execute examples/josh-allen/showcases/test-failure-minimization.allen.` |
| Incident triage | Model hypothesis and user approval | `Execute examples/josh-allen/showcases/incident-triage.allen.` |
| Invoice reconciliation | Typed extraction and agent message | `Execute examples/josh-allen/showcases/invoice-reconciliation.allen.` |
| Deployment risk gate | Child-agent review and user approval | `Execute examples/josh-allen/showcases/deployment-risk-gate.allen.` |
| Customer operation | Agent judgment and dry-run tool call | `Execute examples/josh-allen/showcases/bulk-customer-operation-planning.allen.` |
| Infrastructure drift | Agent plan, user approval, and dry-run tool call | `Execute examples/josh-allen/showcases/infrastructure-drift-remediation.allen.` |

The fixtures stay inside the source files because the MCP bridge does not
grant filesystem, shell, network, transcript, permission, cloud, CRM, or
deployment access. These examples test policy and callback flow. They do not
perform external changes.

The requests under `prompts/` omit JOSH/ALLEN mechanics on purpose. They test
whether an installed agent recognizes a bounded workflow, writes the program,
and runs it through JOSH.
