'use strict';
const fastify = require('fastify')();
fastify.get('/', async () => ({ ok: true }));
fastify.get('/hello/:name', async (req) => ({ hello: req.params.name }));
fastify.post('/echo', async (req) => ({ echo: req.body }));
fastify.listen({ port: 8080, host: '127.0.0.1' }).then(() => console.log('ready'));