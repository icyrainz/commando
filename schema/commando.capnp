@0xb0eb2ca2d3b76d2a;

interface Authenticator {
  challenge @0 () -> (nonce :Data);
  authenticate @1 (hmac :Data) -> (agent :CommandAgent, agentVersion :Text);
}

interface CommandAgent {
  exec @0 (request :ExecRequest) -> (result :ExecResult);
  ping @1 () -> (pong :PingResult);
}

struct ExecRequest {
  command @0 :Text;
  workDir @1 :Text;
  timeoutSecs @2 :UInt32;
  extraEnv @3 :List(EnvVar);
  requestId @4 :Text;
}

struct EnvVar {
  key @0 :Text;
  value @1 :Text;
}

struct ExecResult {
  stdout @0 :Data;
  stderr @1 :Data;
  exitCode @2 :Int32;
  durationMs @3 :UInt64;
  timedOut @4 :Bool;
  truncated @5 :Bool;
  requestId @6 :Text;
}

struct PingResult {
  hostname @0 :Text;
  uptimeSecs @1 :UInt64;
  shell @2 :Text;
  version @3 :Text;
}
