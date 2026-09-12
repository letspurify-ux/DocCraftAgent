export function handleRequest(name) {
  if (!name.trim()) throw new Error('name_required');
  return `Hello, ${name}`;
}
