import { AudienceType, type RayfinContext } from '@microsoft/fabric-user-data-functions';

import { udf } from '../udf.js';

// A function expression.
udf.func(
  'legacyStyle',
  async function (id: number, ctx: RayfinContext<Record<string, unknown>, AudienceType.Fabric>) {
    return `${id}${typeof ctx}`;
  },
);

// A named, synchronous function expression.
udf.func('syncHandler', function named(flag: boolean) {
  return !flag;
});

// A synchronous arrow function.
udf.func('arrowNoAsync', (x: string) => x.toUpperCase());
