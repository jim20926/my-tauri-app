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
const watchProgress = new Map();
const maxJsonBodyBytes = 1024 * 1024;

function sendJson(response, statusCode, body) {
  response.writeHead(statusCode, {
    "Content-Type": "application/json; charset=utf-8",
    "Access-Control-Allow-Origin": "*",
  });
  response.end(JSON.stringify(body));
}

function readJsonBody(request) {
  return new Promise((resolveBody, rejectBody) => {
    let body = "";
    request.setEncoding("utf8");
    request.on("data", (chunk) => {
      body += chunk;
      if (Buffer.byteLength(body, "utf8") > maxJsonBodyBytes) {
        rejectBody(new Error("Request body too large"));
        request.destroy();
      }
    });
    request.on("end", () => {
      try {
        resolveBody(JSON.parse(body));
      } catch {
        rejectBody(new Error("Invalid JSON"));
      }
    });
    request.on("error", rejectBody);
  });
}

function isValidProgressRecord(record) {
  return Boolean(
    record &&
      typeof record.videoId === "string" &&
      record.videoId.length > 0 &&
      typeof record.positionSeconds === "number" &&
      Number.isFinite(record.positionSeconds) &&
      record.positionSeconds >= 0 &&
      typeof record.durationSeconds === "number" &&
      Number.isFinite(record.durationSeconds) &&
      record.durationSeconds > 0 &&
      typeof record.completed === "boolean" &&
      typeof record.updatedAt === "string" &&
      record.updatedAt.length > 0
  );
}

async function handleWatchProgress(request, response) {
  if (request.method === "GET") {
    sendJson(response, 200, {
      version: 1,
      progress: Array.from(watchProgress.values()),
    });
    return;
  }

  try {
    const payload = await readJsonBody(request);
    if (!payload || payload.version !== 1 || !Array.isArray(payload.progress)) {
      sendJson(response, 400, { error: "Invalid watch progress payload" });
      return;
    }

    for (const record of payload.progress) {
      if (!isValidProgressRecord(record)) {
        sendJson(response, 400, { error: "Invalid watch progress record" });
        return;
      }
      const existing = watchProgress.get(record.videoId);
      if (!existing || record.updatedAt >= existing.updatedAt) {
        watchProgress.set(record.videoId, {
          videoId: record.videoId,
          positionSeconds: Math.min(record.positionSeconds, record.durationSeconds),
          durationSeconds: record.durationSeconds,
          completed: record.completed,
          updatedAt: record.updatedAt,
          syncState: "synced",
          lastSyncedAt: new Date().toISOString(),
        });
      }
    }

    sendJson(response, 200, {
      version: 1,
      progress: Array.from(watchProgress.values()),
    });
  } catch {
    sendJson(response, 400, { error: "Invalid JSON body" });
  }
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
  response.setHeader("Access-Control-Allow-Methods", "GET, HEAD, PUT, OPTIONS");
  response.setHeader("Access-Control-Allow-Headers", "Content-Type");

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

  if (requestUrl.pathname === "/health" && ["GET", "HEAD"].includes(request.method ?? "")) {
    sendJson(response, 200, { status: "ok" });
    return;
  }

  if (requestUrl.pathname === "/watch-progress" && ["GET", "PUT"].includes(request.method ?? "")) {
    await handleWatchProgress(request, response);
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
