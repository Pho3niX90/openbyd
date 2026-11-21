// Checks a packed npm package in a project of its own: the add-on for this
// platform loads from the package, and a client reaches the network layer.
// The release workflow runs it on every platform before publishing.
//
//   npm install ./openbyd-<version>.tgz && node smoke.cjs <version>

const assert = require("node:assert/strict");
const { BydTransportError, Client, decodeEnvelope, encodeEnvelope, parseModel, version } = require("openbyd");

async function main() {
  const expected = process.argv[2];
  assert.equal(version(), expected, "the add-on is this release's");
  assert.equal(require("openbyd/package.json").version, expected);

  const envelope = encodeEnvelope('{"hello":"world"}');
  assert.equal(Buffer.from(decodeEnvelope(envelope)).toString("utf8"), '{"hello":"world"}');
  assert.equal(parseModel("GpsInfo", { latitude: 52.0, longitude: 4.0 }).latitude, 52);

  // Nothing listens on port 9: the request is built, enveloped and sent, and the connection is refused.
  const client = new Client({ username: "user@example.com", password: "secret", baseUrl: "http://127.0.0.1:9" });
  try {
    await assert.rejects(client.login(), BydTransportError);
  } finally {
    client.close();
  }
  console.log(`openbyd ${expected} works on ${process.platform}-${process.arch} (Node ${process.version})`);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
