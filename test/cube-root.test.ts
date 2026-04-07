import fc from 'fast-check';
import { Field, Provable, initializeBindings } from 'o1js';
import { cubeRoot64Main } from '../src/cube-root.js';

const MAX_64BIT = (1n << 64n) - 1n;

async function main() {
  await initializeBindings();

  await fc.assert(
    fc.asyncProperty(fc.bigInt({ min: 0n, max: MAX_64BIT }), async (yBig) => {
      const xBig = yBig ** 3n;
      await Provable.runAndCheck(() => {
        const x = Provable.witness(Field, () => Field(xBig));
        const y = Provable.witness(Field, () => Field(yBig));
        cubeRoot64Main(x, y);
      });
    }),
    { numRuns: 10, verbose: true }
  );

  console.log('All properties passed.');
}

await main();
