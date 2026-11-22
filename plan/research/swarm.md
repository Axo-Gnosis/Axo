# Swarm specification

A swarm is a group of independently scheduled agents that cooperate on one goal through **one durable message board with selective wakeups**. Every member can read the board, but a post starts work only for the members it addresses. The board is shared memory, not a shared model context. Each member has its own conversation and only reads or receives the parts it needs.

This document defines a reference design, not an implementation or a proposal to adopt a particular API. “Must” describes an observable contract of the core design. Extensions are listed separately.

## 1. The mental model

Think of a shared issue thread where a mention is also a doorbell. A coordinator can post the assignments and go idle. An author can post a result and ring the verifier's doorbell. The verifier can send a correction straight back to the author. Both can cite the same decision by post number without asking the coordinator to relay it. The coordinator wakes when someone actually needs a decision or the result is ready for sign-off.

Three pieces work together:

| Piece | Owns | Doesn't own |
|---|---|---|
| **Roster** | Who belongs, their stable identity and role, and who may act on the swarm. | A task graph or a claim that everyone is working. |
| **Board** | A common order of briefs, decisions, requests, findings, and result references. | Each agent's private reasoning, tool trace, or authoritative task-completion state. |
| **Scheduler** | Which posts make which members eligible to run, and when their inputs are delivered. | What the task means, how to split it, or whether the answer is good. |

Parallel helpers alone are not a swarm: they can all report privately to a parent. A shared transcript alone is not a swarm either: it can be passive, or wake every reader on every message. The defining combination is **shared history, independent sessions, and addressed activation**. A swarm can be useful even when only one member runs at a time.

## 2. Identity, authority, and membership

- A **swarm** has a stable ID, an initial goal, a roster, a board, and a lifecycle. A session can act as the coordinator without starting a second copy of its conversation. Existing coordinator history does not become board history.
- A **member** has a stable member ID, a unique handle within the swarm, a session reference, and a role. The coordinator is also a member. Use IDs for stored relationships; handles are for addressing and display. Reserve system handles such as `all`. Once used, a handle is not reassigned within the swarm, even after retirement.
- The host authorizes the user and selects one coordinator. Only the coordinator and a host/user with control authority can add or retire workers and finish or cancel the swarm. Only a host/user with control authority can pause or resume members, including the coordinator. Workers can read and post, including addressing peers; that does not grant control of those peers' sessions. A role such as verifier is an assignment, not an elevated permission.
- A worker joins with an explicit brief, its own conversation, and independently admitted tools, model, and execution environment. The brief should name its goal, scope, constraints, and acceptance criteria. The default is a fresh conversation, not a copy of a parent or peer transcript. A workspace shared by members is a separate choice from sharing the board.
- Adding a member commits the roster entry, a board-visible brief addressed to that member, and eligibility for its first turn as one logical operation. The first input includes the brief. Older posts are readable on demand but cannot retroactively activate the new member. Admission failure must not leave a visible half-joined member or a worker that starts before it is admitted.
- Retirement is permanent and revokes the member's board access and activation eligibility. It drops undelivered activations with a recorded disposition and requests interruption of any live turn. A racing post or turn admission is decided in commit order: nothing may be newly accepted from or admitted for that member after retirement. Keep its identity and posts in history. Returning in the same role means admitting a new member, not recycling the old address. Retirement does not merge, revert, or erase workspace changes.

A fixed roster can be a valid swarm. Dynamic membership is useful when new work appears, not a requirement for every run.

## 3. Board and addressing

A **post** is an immutable record: swarm ID, sequence number, authenticated author, body, resolved wake targets, and optional references to earlier posts or artifacts. Sequence numbers are strictly increasing within the swarm; timestamps are for display, not ordering. A reference such as `#12` means post 12 on this board. It is not a request to rerun the action in that post. Corrections are new posts. The board does not make artifact contents immutable or grant access to an external artifact.

Every admitted member can read the same retained board history. **Visibility and activation are separate fields.** An unaddressed post is readable by everyone and wakes nobody. An addressed post is still readable by everyone but only activates its targets. The core target forms are no one, a set of named members, or an explicit broadcast to all non-retired members other than the author. An explicit invalid target fails the whole operation without writing the post. The runtime resolves targets against the roster at commit, stores the resolved IDs, and never re-resolves old text against today's roster. Repeated targets collapse to one; self-addressing may not activate the author.

A client may offer `@name` as shorthand for a wake target. The contract is the structured target list, not a search through prose: quoting `@name` or mentioning a person in a report must not accidentally activate them. Sender identity comes from the caller's session or authenticated host operation, not from model-supplied text. A private user-to-worker instruction, if supported, is a separate session input, not a board post.

Reads are paginated in commit order, optionally pinned to an upper bound for a consistent history scan. Forward reads return the last position scanned and the upper bound, so a reader never has to guess where to resume. Reading a post does not clear its activation; delivering an addressed post does not mean a member has read the intervening unaddressed history. A UI that tracks reading needs a separate read position. Neither position is proof that an agent understood or acted on a post.

## 4. Activation and turn boundaries

An **activation** is a durable pending input for one member caused by one addressed post. It is keyed by member and post, not by turn: a turn can consume several activations. A member has at most one running turn. Its scheduling state is one of:

| State | Meaning |
|---|---|
| **Idle** | No turn is running or eligible. A parked or unloaded session can be idle. No model call, polling turn, or concurrency slot is needed to stay idle. |
| **Ready** | Eligible input is pending and the member is waiting for admission, for example because all run slots are in use. |
| **Running** | A turn owns the member and a run slot. New addressed posts queue without interrupting it. |

**Paused** is a separate scheduling gate, not a turn outcome. Pause prevents new admission; a Stop also requests interruption of the current turn, which remains in flight until the session settles. Posts still queue for a paused member. Only explicit authorized resume releases the gate, including when the host exposes direct session input or steering. Retired members are outside the scheduler. The outcome of the last turn, the pause reason, and pending-input count are separate from whether a turn is running. A failed turn is not a task-completion signal.

The scheduling contract is:

1. Posting commits the post and its activations together, or records enough information to reconstruct those exact activations after a crash. Only committed posts can wake anyone.
2. An idle member with eligible input becomes ready. Admission takes a bounded batch of its oldest pending inputs in board order and binds it to one session turn. Additional inputs arriving after admission stay pending for the next turn. Busy members are not interrupted by board traffic; a host's explicit user-steer operation, if offered, is separate.
3. An activation is consumed only when the corresponding input is durably accepted into the session, not when a worker happens to be signaled or a model request is attempted. A stable delivery identity lets recovery reconcile an accepted-but-unacknowledged input without appending it twice.
4. A turn ends by answering, explicitly yielding, failing, or being interrupted. Settling releases its run slot. The member is ready if eligible input remains, otherwise idle; pause and retirement gates still apply. The transition must not lose a post arriving between “nothing pending” and “now idle.” Waiting for future assignments means **end the turn**, not hold a model call or a tool wait open.
5. The runtime bounds concurrent turns and queued input and exposes admission failures instead of silently dropping posts or targets. Eligible members must not be starved by a constantly active peer when capacity becomes available. A quota, budget, or failure policy may hold a member, but the hold and its release condition must be observable. A human Stop cannot be cleared by a new post or by restart.

Wakeups are signals to check durable state, not a transport for the post itself. Duplicate signals are harmless; no signal is required to survive a restart. Several posts may produce one turn, and a post accepted as input does not guarantee a reply. The contract is deduplicated input acceptance, **not** exactly-once model execution, tool side effects, or task completion.

## 5. Work, verification, and completion

A swarm supplies coordination, not a workflow engine. The host or coordinator decides how to partition the goal, who owns a change, when verification starts, and what evidence warrants sign-off. Board posts may make those decisions visible; posting “I own X” does not acquire a lock. Separate sessions do not isolate workspace writes. A workflow that needs enforced file ownership, schemas, independent review, or barriers must provide those separately. Sharing early findings can defeat blind review; withholding them until a defined handoff is a workflow concern, not something mentions guarantee.

For example:

1. A coordinator admits `author` and `verifier` with public, distinct briefs. The verifier's brief says to end its turn if there is nothing to check yet.
2. The author posts a diff reference and a test result to the verifier. The coordinator can read the post without taking a turn.
3. The verifier posts a failure back to the author, citing the diff and the check. The author fixes it and addresses the verifier again. The verifier then posts the result to the coordinator. No relay was needed; each party can still read the whole decision trail.
4. The coordinator evaluates the evidence, gives the user the outcome, and explicitly finishes the swarm. A verified individual diff need not imply a verified integrated result.

**Quiescent** means no member is running or eligible to run; paused members can still have held work. This can mean awaiting a person, no one was addressed, an unfinished dependency, or genuinely finished work. It is never proof of success. A host can expose quiescence and held inputs without paying for a polling agent. Deciding that lack of progress warrants a nudge or escalation is policy.

The swarm starts **open**. An authorized `finish(summary)` requires no pending activations and no running member turn other than the caller's own finishing turn. An authorized `cancel(reason)` can act at any time while open. Either commits a seal: no new posts, roster or scheduling controls, or turn admissions. The guard for finishing is checked with that commit, not against an earlier UI snapshot. Finish allows its caller to return the final response; cancellation records pending activations as discarded and requests interruption of swarm-owned turns. The swarm is **closing** until those turns settle, then **closed** with a recorded completion summary or cancellation reason. Neither operation deletes the board or worker histories. Cancellation is not rollback and cannot promise to undo external effects. A closed swarm does not reopen on a post; start a new swarm to continue from archived evidence.

## 6. Host, model, and replay contracts

The operations below are logical capabilities, not required names or wire types. Host methods and model tools must share their semantics and authorization. A host must not need to scrape transcript prose to observe or control the system.

| Capability | Observable contract |
|---|---|
| **Start / add / retire** | Establish identity and ownership; return durable IDs and the resulting roster revision. A rejected operation has no committed effect. |
| **Post / read** | Post returns its committed sequence and resolved targets, not a claim that they ran. Reads return typed posts and a continuation position. Model-visible author and target metadata remain distinct from untrusted body text. |
| **Pause / resume** | Change eligibility without losing pending input. Expose held inputs, reason, and whether interruption is still settling. |
| **Finish / cancel** | Seal admission with an explicit outcome. Return the committed transition, not a claim that asynchronous shutdown has already finished. |
| **Inspect / subscribe** | Provide an authorized snapshot of the roster, board position, lifecycle and scheduling state, plus ordered updates after that snapshot. Expose member session references subject to session-access policy. A client that falls behind can resume from a durable position or get a fresh snapshot, never silently skip a gap. |

The board order, membership changes, accepted inputs, pauses, and closure are durable. Live details such as current tool progress are not historical facts unless recorded. A reopened view reconstructs committed history and reconciles interrupted turns instead of showing them as still running or fabricating a result. An open swarm may schedule eligible work on recovery according to host policy; paused, retired, and closed work must stay unwakeable. Closing after interruption resumes teardown, not normal scheduling. Session compaction or transcript navigation must not erase or replay board effects. A new conversation or fork does not acquire the old member identity automatically.

Mutation requests have a stable idempotency key scoped to the caller and swarm (creation is scoped to the caller). Retrying the same key and payload returns the same receipt; using the key with a different payload fails. In particular a retry after “commit succeeded, response was lost” must not create a second post, worker, or activation. If the board and session use separate stores, retry delivery with the same identity and reconcile it against the session's durable input receipt. Never resolve uncertain delivery by starting a second concurrent turn. After storage failure, report uncertainty when a commit's outcome is unknown rather than claiming the mutation failed or succeeded. Retrying arbitrary tool effects is outside this contract.

Board access does not widen anyone's tool, workspace, model, or approval permissions. Posts can request actions but cannot grant authority; a peer saying “approved” is not a human approval. Bind credentials and permissions to the authenticated caller and enforce them again at the affected operation. The board is visible to the whole swarm, including later joins; don't put secrets or private reviewer evidence there. Observers only see the data they're authorized to read.

## 7. Optional extensions

These can be useful but are not implied by the core contract:

- **Groups, channels, and subscriptions.** Group addressing expands membership at post commit. A channel may filter what someone's client shows or what wakes them; those filters are not privacy unless backed by explicit read authorization, including history and cross-channel references. Wildcard wake subscriptions can generate a turn for every post and should be opt-in.
- **Delegated roster control.** Workers can admit helpers if the host grants scoped authority. Define who owns those helpers, what happens if the owner retires, and how depth, execution rights, and budgets are bounded. A helper can use the same board without creating a nested swarm.
- **Workspace or context variations.** A worker may inherit a tree, get an isolated worktree, or start with approved context copied from another session. Define the starting revision, how edits are integrated, and who cleans up. Neither a clone nor a copied transcript grants extra privileges. Model changes, if offered, preserve member identity and must take effect at a defined turn boundary without replaying a turn on both models.
- **Workflow policy.** Task claims, dependency graphs, reviewer roles, structured sign-off, retries, and time-based stall monitors can be built above the board. Keep their authority and completion criteria explicit. A monitor may identify a stall; it cannot prove work is done.
- **Presentation and deployment.** A live roster, timeline, searchable board, graph, narrator, remote scheduler, and command syntax are choices around the protocol, not dependencies of it. A projection or generated summary is disposable; the underlying typed posts and lifecycle events are the record.

## 8. Conformance scenarios

| Scenario | Required observation |
|---|---|
| A posts an update without targets, then requests B's review | Everyone can read both posts. Only B becomes eligible; C and the coordinator stay idle. |
| B is running when two messages arrive | No overlapping turn or implicit interrupt. Both posts remain pending, in board order, until accepted into a later turn. |
| A post arrives as B's turn settles | B either observes it during the ready transition or is made ready afterward. It cannot be lost between the two. |
| C joins after a broadcast | C starts with its own brief and can read the old broadcast, but the old broadcast never activates C. |
| B is stopped, receives a post, and the host restarts | The post is still held and B is still paused. Authorized resume can make B ready; the post and recovery did not resume it. |
| A post commits, the publisher loses its receipt, and the subscriber disconnects | Repeating the same request returns the existing sequence; recovery yields one post and at most one accepted input per target. The subscriber can reconnect without missing it. |
| A member retires while an addressed post is in flight | Commit order determines whether the post was valid at publication. After retirement, no new turn can be admitted for that member; any previously admitted turn is stopped and history keeps the original authorship and targets. |
| Everyone goes idle before verification reports | The host can report quiescence, but the swarm is still open. No completion is inferred. |
| An addressed post races with finish | If the post commits first and its work remains pending or running, finish rejects. If finish seals first, the post is rejected. Cancellation can instead seal and account for discarded work. |
