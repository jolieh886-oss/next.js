'use client'

// eslint-disable-next-line no-undef
__turbopack_emit__('./unique.js', {
  namespace: 'my-test',
  data: 'data-for-unique-client-b',
})

// eslint-disable-next-line no-undef
__turbopack_emit__('../shared-app-client.js', {
  namespace: 'my-test',
  data: 'data-for-shared-app-client-b',
  scope: 'app',
})
