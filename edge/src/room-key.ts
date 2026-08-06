const ID_RE = /^[A-Za-z0-9_-]{1,128}$/;

export const scopedSessionRoomKey = (
  projectId: string,
  deploymentId: string,
  sessionId: string
): string => {
  if (![projectId, deploymentId, sessionId].every((value) => ID_RE.test(value))) {
    throw new Error("invalid_session_room_scope");
  }
  return `s4/${projectId}/${deploymentId}/${sessionId}`;
};
