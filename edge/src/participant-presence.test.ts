import { describe, expect, it } from "vitest";
import { isValidParticipantCursor } from "./session-room";

describe("ephemeral participant cursor", () => {
  it("accepts bounded UTF-8 caret and selection boundaries", () => {
    expect(
      isValidParticipantCursor(
        { targetId: "publication/message-1", caret: 3, selection: { start: 1, end: 3 } },
        "aéz"
      )
    ).toBe(true);
  });

  it("rejects split codepoints, reversed ranges, and oversized targets", () => {
    expect(isValidParticipantCursor({ targetId: "message-1", caret: 2 }, "aéz")).toBe(false);
    expect(
      isValidParticipantCursor({
        targetId: "message-1",
        caret: 2,
        selection: { start: 3, end: 1 }
      })
    ).toBe(false);
    expect(isValidParticipantCursor({ targetId: "x".repeat(257), caret: 0 })).toBe(false);
  });
});
