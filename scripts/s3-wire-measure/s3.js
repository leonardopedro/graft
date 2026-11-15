// Minimal S3-compatible server for the wire measurement: s3rver is file-backed
// and implements the subset Graft uses (PUT/GET with ranges, HEAD, list).
const S3rver = require("s3rver");
const path = require("path");

const port = Number(process.env.S3_PORT || 9000);
const directory = process.env.S3_DIR || path.join(__dirname, "s3-data");
const bucket = process.env.S3_BUCKET || "graft-bucket";

new S3rver({
  port,
  address: "127.0.0.1",
  directory,
  configureBuckets: [{ name: bucket }],
}).run();
console.log(`s3rver on 127.0.0.1:${port}, bucket ${bucket}, data ${directory}`);
