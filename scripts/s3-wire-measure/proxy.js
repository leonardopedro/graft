// Request-logging HTTP proxy in front of an S3 server.
//
// Every client request is forwarded as its own upstream request, so the log has
// one entry per S3 operation with the exact bytes that went back on the wire
// (status line + headers + body). Graft fetches a page by range-requesting the
// frame that holds it, so each `206` here corresponds to one cached frame.
const http = require("http");
const fs = require("fs");

const [listenPort, upstreamPort, logPath] = [
  Number(process.argv[2] || 9001),
  Number(process.argv[3] || 9000),
  process.argv[4] || "wire.log",
];
// One-way delay added in each direction, to stand in for the RTT a real bucket
// adds. A request therefore costs roughly 2x this on top of its transfer time.
const delayMs = Number(process.argv[5] || 0);

const log = [];
const agent = new http.Agent({ keepAlive: false });

const server = http.createServer((req, res) => {
  const forward = () => {
  const started = Date.now();
  const upstream = http.request(
    {
      host: "127.0.0.1",
      port: upstreamPort,
      method: req.method,
      path: req.url,
      headers: { ...req.headers, host: `127.0.0.1:${upstreamPort}` },
      agent,
    },
    (up) => {
      // Paused so the byte counter does not consume what the delayed pipe is
      // supposed to send to the client.
      up.pause();
      let bodyBytes = 0;
      up.on("data", (chunk) => {
        bodyBytes += chunk.length;
      });
      up.on("end", () => {
        const headBytes = Buffer.byteLength(
          `HTTP/1.1 ${up.statusCode} ${up.statusMessage}\r\n` +
            rawHeaderBytes(up.rawHeaders) +
            "\r\n",
        );
        log.push({
          method: req.method,
          path: req.url,
          range: req.headers.range || null,
          status: up.statusCode,
          requestBytes: Number(req.headers["content-length"] || 0),
          responseBytes: headBytes + bodyBytes,
          bodyBytes,
          ms: Date.now() - started,
        });
        fs.writeFileSync(logPath, JSON.stringify(log, null, 1));
      });
      const respond = () => {
        res.writeHead(up.statusCode, up.statusMessage, up.headers);
        up.pipe(res);
      };
      if (delayMs) setTimeout(respond, delayMs);
      else respond();
    },
  );
  upstream.on("error", (err) => {
    res.writeHead(502);
    res.end(String(err));
  });
  req.pipe(upstream);
  };
  if (delayMs) setTimeout(forward, delayMs);
  else forward();
});

function rawHeaderBytes(raw) {
  let n = 0;
  for (let i = 0; i < raw.length; i += 2) {
    n += Buffer.byteLength(`${raw[i]}: ${raw[i + 1]}\r\n`);
  }
  return n;
}

server.listen(listenPort, "127.0.0.1", () =>
  console.log(`proxy ${listenPort} -> ${upstreamPort}, logging to ${logPath}`),
);
