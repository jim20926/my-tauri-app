import { createReadStream } from "node:fs";
import { readFile, stat } from "node:fs/promises";
import { createServer } from "node:http";
import { fileURLToPath } from "node:url";
import { dirname, extname, join, resolve, sep } from "node:path";

const serverDirectory = dirname(fileURLToPath(import.meta.url));
const mediaDirectory = resolve(join(serverDirectory, "media"));
const port = Number(process.env.MOCK_SERVER_PORT ?? 3001);
const mimeTypes = {
  ".json": "application/json; charset=utf-8",
  ".mp4": "video/mp4",
};

function sendJson(response, statusCode, body) {
  response.writeHead(statusCode, {
    "Content-Type": "application/json; charset=utf-8",
    "Access-Control-Allow-Origin": "*",
  });
  response.end(JSON.stringify(body));
}

function isSafeMediaName(filename) {
  return Boolean(filename) && filename === join(filename) && !filename.includes("/") && !filename.includes("\\");
}

async function serveVideo(request, response, filename) {
  if (!isSafeMediaName(filename)) {
    sendJson(response, 400, { error: "Invalid media path" });
    return;
  }

  const filePath = resolve(join(mediaDirectory, filename));
  if (filePath !== mediaDirectory && !filePath.startsWith(`${mediaDirectory}${sep}`)) {
    sendJson(response, 403, { error: "Forbidden" });
    return;
  }

  let fileStats;
  try {
    fileStats = await stat(filePath);
  } catch {
    sendJson(response, 404, { error: "Media not found" });
    return;
  }

  const baseHeaders = {
    "Accept-Ranges": "bytes",
    "Access-Control-Allow-Origin": "*",
    "Content-Type": mimeTypes[extname(filePath).toLowerCase()] ?? "application/octet-stream",
  };
  const range = request.headers.range;
  let start = 0;
  let end = fileStats.size - 1;
  let statusCode = 200;

  if (range?.startsWith("bytes=")) {
    const [rangeStart, rangeEnd] = range.slice(6).split("-");
    start = Number.parseInt(rangeStart, 10);
    end = rangeEnd ? Number.parseInt(rangeEnd, 10) : end;

    if (!Number.isInteger(start) || !Number.isInteger(end) || start < 0 || start > end || end >= fileStats.size) {
      response.writeHead(416, {
        ...baseHeaders,
        "Content-Range": `bytes */${fileStats.size}`,
      });
      response.end();
      return;
    }

    statusCode = 206;
  }

  const contentLength = end - start + 1;
  response.writeHead(statusCode, {
    ...baseHeaders,
    "Content-Length": contentLength,
    ...(statusCode === 206 ? { "Content-Range": `bytes ${start}-${end}/${fileStats.size}` } : {}),
  });

  if (request.method === "HEAD") {
    response.end();
    return;
  }

  createReadStream(filePath, { start, end }).pipe(response);
}

const server = createServer(async (request, response) => {
  response.setHeader("Access-Control-Allow-Origin", "*");
  response.setHeader("Access-Control-Allow-Methods", "GET, HEAD, OPTIONS");

  if (request.method === "OPTIONS") {
    response.writeHead(204);
    response.end();
    return;
  }

  const requestUrl = new URL(request.url ?? "/", `http://${request.headers.host ?? `localhost:${port}`}`);

  if (requestUrl.pathname === "/videos.json" && ["GET", "HEAD"].includes(request.method ?? "")) {
    try {
      const catalog = await readFile(join(serverDirectory, "videos.json"), "utf8");
      response.writeHead(200, { "Content-Type": mimeTypes[".json"] });
      response.end(request.method === "HEAD" ? undefined : catalog);
    } catch {
      sendJson(response, 500, { error: "Catalog unavailable" });
    }
    return;
  }

  if (requestUrl.pathname.startsWith("/media/") && ["GET", "HEAD"].includes(request.method ?? "")) {
    await serveVideo(request, response, decodeURIComponent(requestUrl.pathname.slice("/media/".length)));
    return;
  }

  sendJson(response, 404, { error: "Not found" });
});

server.listen(port, "127.0.0.1", () => {
  console.log(`Mock media server listening at http://localhost:${port}`);
  console.log(`Catalog: http://localhost:${port}/videos.json`);
});

function shutdown() {
  server.close(() => process.exit(0));
}

process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);
