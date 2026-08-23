# PD-7: typed streams and multimodal artifacts

Status: Proposed

Depends on: [PD-3](PD-3.md), [PD-6](PD-6.md)

Back to the [roadmap](../../ROADMAP.md#proposed).

## Decision summary

Add an affine `Stream<T>` type with bounded buffering and backpressure. Add
typed artifact references for images, audio, video, documents, and attachments.

Keep content access separate from reference access. A reference does not grant
permission to read its content.

## Problem

ALLEN `0.1` provider operations return one completed value. This model requires
the runtime to buffer a complete response before source can use it.

Large model responses, tool progress, browser observations, audio, and document
content do not fit this model well. Tool-specific streaming conventions also
reduce portability and weaken replay.

Multimodal prompt content is not part of the current language. Programs need a
typed way to refer to non-text content without placing raw bytes in every
prompt or protocol message.

## Scope

This proposal adds:

- An affine `Stream<T>` value.
- Pull-based backpressure.
- Bounded item and byte buffers.
- Typed stream completion and failure.
- Source-controlled stream cancellation.
- Optional resumable stream positions.
- Typed artifact references.
- Explicit artifact content capabilities.
- Multimodal prompt parts.
- Recording and replay for stream items and artifact metadata.

This proposal does not add ambient access to local files or remote objects.

## Terms

A stream is an ordered sequence of typed items.

Backpressure prevents a producer from exceeding the accepted buffer.

An artifact is external content with typed metadata and stable identity.

An artifact reference identifies content. It does not contain or grant the
content.

A content capability permits one declared content operation.

## Concrete example: Twilio call transcription

This example transcribes one support call. Twilio supplies audio frames and a
final recording artifact. Amazon Transcribe supplies partial and final text.

The workflow stores the final transcript in Amazon S3. It posts that document
to Slack after policy approval.

The tool and event names are proposed ALLEN adapters.

| Proposal concept | Named service, tool, or event | Concrete use |
|---|---|---|
| Input stream | `twilio.media.stream.started@1` | Open one Twilio call-audio stream. |
| Stream consumer | `tools.aws_transcribe.streaming.start@1` | Convert audio frames to typed transcript events. |
| Backpressure | Twilio adapter receive window | Limit unprocessed audio frames. |
| Resume limit | Amazon Transcribe streaming adapter | Declare that this adapter has no resumable cursor. |
| Resumable position | `tools.apache_kafka.consume@1` | Resume an audit stream from one committed Kafka offset. |
| Audio artifact | `twilio.recording.completed@1` | Receive the final call recording as a typed artifact. |
| Document artifact | `tools.aws_s3.put_object@1` | Store the final transcript document. |
| Image artifact | `zendesk.ticket.attachment_added@1` | Receive a screenshot for the linked support ticket. |
| Multimodal analysis | `tools.google_cloud_vision.annotate_image@1` | Inspect an attached screenshot. |
| Approval event | `slack.interaction.transcript_approved@1` | Approve one exact transcript digest. |
| Final sink | `tools.slack.files.upload@1` | Upload the approved transcript to Slack. |

Illustrative future syntax follows. This syntax is not valid ALLEN `0.1`.

```allen
let audio: Stream<TwilioAudioFrame> =
  await event.open_stream("twilio.media.stream.started@1")?;

let transcript: Stream<AmazonTranscriptEvent> =
  await tools.aws_transcribe.streaming.start.call({
    audio,
    language: "en-US",
    max_buffer_frames: 50,
  })?;

mut final_segments: List<TranscriptSegment> = [];

await transcript.for_each(fn(event: AmazonTranscriptEvent) returns Void {
  match event {
    Partial { text: _ } => Void,
    Final { segment } => final_segments.push(segment),
  }
});

let recording = await event.wait<TwilioRecordingCompleted>({
  name: "twilio.recording.completed@1",
  subject: event.call_id,
})?;

let document = make_transcript(final_segments, recording.audio_artifact);
let stored = await tools.aws_s3.put_object.call({
  bucket: "support-call-artifacts",
  key: event.call_id + "/transcript.json",
  value: encode(document),
})?;

let approval = await event.wait<SlackTranscriptApproval>({
  name: "slack.interaction.transcript_approved@1",
  subject_digest: digest(document),
})?;

await tools.slack.files.upload.call({
  approval: approval.receipt,
  channel: "support-review",
  artifact: stored.artifact_reference,
})?;
```

Partial Amazon Transcribe text cannot become the final `Transcript` value. The
runtime accepts only final segments for that value.

The Amazon S3 reference does not grant source permission to read the object.
Slack receives the artifact only after the sink policy accepts its labels.

If Zendesk adds a screenshot, `zendesk.ticket.attachment_added@1` supplies its
image reference. The workflow passes that reference to Google Cloud Vision. It
does not place raw image bytes in the prompt or Slack message.

If Twilio sends frames faster than Amazon Transcribe accepts them, the adapter
closes its receive window. The runtime ends the stream when the bounded buffer
still exceeds policy.

This Amazon Transcribe adapter declares no resumable cursor. A host restart
cannot continue the old transcription session. A durable workflow must retain
the S3 audio artifact and start a new transcription operation.

The Apache Kafka adapter gives a contrasting case. Its cursor binds the topic,
partition, consumer group, and committed offset. A workflow can resume that
audit stream only when the manifest grants the same Kafka scope.

## Stream type

`Stream<T>` is affine. Source can move it, read it, or cancel it. Source cannot
copy it or place it in an unrestricted aggregate.

Illustrative future syntax follows. This syntax is not valid ALLEN `0.1`.

```allen
let stream: Stream<AmazonTranscriptEvent> =
  await tools.aws_transcribe.streaming.start.call({
    audio: twilio_audio,
    language: "en-US",
    max_buffer_frames: 50,
  })?;

await stream.for_each(fn(event: AmazonTranscriptEvent) returns Void {
  match event {
    Partial { text } => report_progress(text),
    Final { segment } => store(segment),
  }
});
```

The final item does not replace stream completion. The provider contract states
whether it requires one final typed value.

## Stream states

A stream has these states:

```text
open -> completed
     -> failed
     -> cancelled
```

Each item has a stable sequence number. The first sequence number is zero. The
provider cannot reuse or skip a sequence number unless its stream contract
permits a documented gap marker.

Only one terminal state is valid. A late item after a terminal state is a
protocol failure.

## Backpressure and limits

The consumer requests or accepts a bounded number of items. The runtime does
not read more provider data when the buffer reaches its limit.

The manifest and host set limits for:

- Item count.
- Item byte size.
- Total decoded bytes.
- Buffer bytes.
- Stream duration.
- Idle time.
- Concurrent streams.

The host can lower each requested limit.

If a provider cannot apply backpressure, the host adapter must buffer within
the same limits or reject that provider mode.

## Cancellation

PD-6 defines cancellation ownership. Dropping a live stream requests
cancellation and starts bounded cleanup.

The runtime discards late items. It records the late-item condition without
making content available to source.

Cancellation of a mutation stream does not prove that the mutation stopped.
PD-2 defines the final mutation state.

## Resumable streams

A provider can declare resumable stream support. It returns an opaque cursor in
each accepted item receipt.

The cursor binds:

- The provider principal.
- The stream request digest.
- The last accepted sequence.
- The result schema.
- The expiration policy.

Source cannot read or change the cursor. A resume request uses the same provider
and request digest unless a migration contract permits another provider.

A cursor is not durable unless the provider and PD-1 workflow contract permit
checkpoint storage.

## Artifact types

The first profile defines these artifact kinds:

```allen
Artifact<Image>
Artifact<Audio>
Artifact<Video>
Artifact<Document>
Artifact<Attachment>
```

An artifact reference includes bounded metadata:

- Artifact ID.
- Artifact kind.
- Media type.
- Byte length when known.
- Content digest when policy permits it.
- Creator principal.
- Origin and security labels from PD-3.
- Creation and expiration data.
- Available content operations.

The reference does not include a native path, credential, or unrestricted URL.

## Content access

The runtime supplies explicit content operations. Examples include bounded byte
read, text extraction, image decode, audio segment read, and metadata query.

Each operation has an effect and a capability. The host checks the artifact
scope before access.

A program can pass a reference to a provider without receiving content bytes.
The sink policy from PD-3 controls that transfer.

## Multimodal prompts

For example, Google Vertex AI Gemini can inspect an Amazon S3 image reference.
The prompt contains a typed image part:

```allen
await models.google_vertex_ai.gemini.request(prompt {
  system: "Inspect the damage in the image.",
  data: { inspection_id },
  content: [PromptPart.Image(photo)],
  output: InspectionResult,
})?;
```

The selected model policy must accept the artifact kind, media type, size, and
security labels before prompt disclosure.

A text-only provider must reject unsupported content. It can use an explicit
conversion tool when the program declares that operation. It must not silently
drop the content.

## Partial and final values

Partial model or tool output remains untrusted and incomplete. Source must not
use a partial value as the final response type.

The provider marks the final candidate. The runtime validates the complete
candidate against the requested schema. Only then can it produce the final
typed value.

Progress text is a separate type. It cannot become a final value through an
implicit conversion.

## Recording and replay

Recording stores:

- Stream identity and provider principal.
- Request and item schema digests.
- Each sequence number and item digest.
- Backpressure requests.
- Terminal state.
- Cancellation events.
- Cursor commitments.
- Artifact metadata and content commitments.

Replay releases items in recorded order. It enforces the recorded buffer and
terminal behavior. It does not read live artifact content unless the replay
contract names a verified immutable content source.

## Security rules

- A stream cannot exceed its owner scope.
- A reference does not grant content access.
- Artifact labels survive prompts, storage, and delegation.
- A sink checks policy before content disclosure.
- Error text does not contain protected content.
- A text provider cannot ignore an unsupported artifact.
- Raw URLs and local paths do not act as artifact credentials.
- Partial output cannot pass final schema validation.

## Failure cases

The runtime must reject these conditions:

- Duplicate or out-of-order sequence numbers.
- An item after stream completion.
- A producer exceeds buffer limits.
- A resume cursor has a different request or provider.
- An artifact kind does not match its metadata.
- A prompt provider cannot accept one content part.
- A content operation lacks artifact authority.
- Replay has a changed item or terminal state.

## Implementation work

1. Add affine `Stream<T>` ownership rules.
2. Add stream instructions and provider messages.
3. Add buffer, byte, duration, and idle limits.
4. Add cancellation through PD-6.
5. Define artifact reference schemas and capability checks.
6. Add multimodal prompt parts and provider matching.
7. Extend recording, replay, events, and redaction.
8. Add hostile producer and large-content tests.

## Acceptance tests

- Open one stream from `twilio.media.stream.started@1`.
- Apply backpressure before the Twilio frame buffer exceeds its limit.
- Send final audio frames to Amazon Transcribe in order.
- Reject partial Amazon Transcribe text as a final transcript.
- Bind `twilio.recording.completed@1` to the same Twilio call ID.
- Store the transcript through the Amazon S3 adapter.
- Upload only an authorized S3 artifact reference to Slack.
- Bind the Slack approval event to the final transcript digest.
- Pass a screenshot reference to Google Cloud Vision without raw byte access.
- Consume a bounded stream without full response buffering.
- Stop producer reads when the buffer reaches its limit.
- Cancel a stream and reject a late item.
- Resume `tools.apache_kafka.consume@1` from an accepted offset cursor.
- Reject a cursor for a different request.
- Pass an image reference without granting byte access.
- Reject a multimodal prompt on a text-only provider.
- Replay the exact item order and terminal state.

## Open decisions

1. Can a stream value cross a durable checkpoint?
2. Which artifact operations belong in the standard library?
3. Can one artifact have more than one immutable representation?
4. How does replay verify large content without storing it?
5. Which partial-result forms should model providers expose?
