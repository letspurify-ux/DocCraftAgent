export function handleRequest(name: string): string {
  if (!name.trim()) throw new Error('name_required');
  return `Hello, ${name}`;
}
