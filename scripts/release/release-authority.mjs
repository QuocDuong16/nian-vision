function isForbiddenHost(hostname) {
  const host = hostname.toLowerCase().replace(/\.$/, "");
  return (
    host === "localhost" ||
    host.endsWith(".localhost") ||
    host === "127.0.0.1" ||
    host === "[::1]" ||
    host === "::1" ||
    host === "example.com" ||
    host.endsWith(".example.com") ||
    host === "example.org" ||
    host.endsWith(".example.org") ||
    host === "example.net" ||
    host.endsWith(".example.net") ||
    host === "example" ||
    host.endsWith(".example") ||
    host === "invalid" ||
    host.endsWith(".invalid") ||
    host === "test" ||
    host.endsWith(".test")
  );
}

export function validateHttpsAuthority(raw, label) {
  let url;
  try {
    url = new URL(raw);
  } catch {
    throw new Error(`${label} must be a valid HTTPS URL`);
  }
  if (url.protocol !== "https:") throw new Error(`${label} must use HTTPS`);
  if (isForbiddenHost(url.hostname)) throw new Error(`${label} is not a production authority`);
  return url;
}
