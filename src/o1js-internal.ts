import { createRequire } from 'module';
import { dirname, resolve } from 'path';

const require = createRequire(import.meta.url);
const o1jsDistNode = dirname(require.resolve('o1js'));

export async function importO1jsInternal(subpath: string) {
  return import(resolve(o1jsDistNode, subpath));
}
