#!/usr/bin/env node
'use strict';

require('./sharepoint.js').launch('sp', process.env.SHAREPOINT_PERSONA || 'sharepoint');
